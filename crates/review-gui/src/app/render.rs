//! Drawing the window: the files, the diff or buffer, the bars and the overlays.
//!
//! What shows and in which colors is [`review_ui::paint`]'s; here is where it goes, in gpui's
//! pixels.
//! Code and our own annotations never share a look. Code rows sit on the editor background with
//! a stable line-number gutter; everything the tool adds (hunk headers, gaps, notes, moves) sits
//! on a distinct band in muted colors, without a gutter.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Decorations, Div, Hsla, ListHorizontalSizingBehavior, MouseButton,
    SharedString, Stateful, StyledText, Window, canvas, div, px, relative, uniform_list,
};
use gpui_component::input::Input;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{h_flex, v_flex, window_border};
use review_ui::frame::List;
use review_ui::keymap::Area;
use review_ui::model::buffer::Marker;
use review_ui::model::{Mode, Overlay, Phase, Updated};
use review_ui::paint::annotation::{self, Annotation, HunkHeader, NotePan, Scope};
use review_ui::paint::bars::{self, HintRoom};
use review_ui::paint::code::{self, At, BufferLines, Frame};
use review_ui::paint::display::display_width;
use review_ui::paint::files::{self, ProgressLine};
use review_ui::paint::header::{self, METER_BAR_CELLS, Meter, StateCard};
use review_ui::paint::{self, Palette, Piece, Room};
use review_ui::rows::{Body, Cell, Note, Row};
use review_ui::text::{Column, Pane};

use super::measure::FRAME;
use super::{ReviewApp, ink, measure, scroll};
use crate::color::to_gpui;

/// The grid every spacing is a multiple of.
pub const GRID: f32 = 4.0;

/// The accent bar along a note's left edge.
const NOTE_ACCENT: f32 = 3.0;

/// A note's parts, as [`Annotation::Note`] has them, and how it moves with the code.
struct NoteRow {
    text: Piece,
    tag: Option<Piece>,
    accent: review_ui::color::Hsla,
    background: review_ui::color::Hsla,
    pan: NotePan,
}

/// A move's, a collapsed unit's or a description's parts, as [`Annotation::Line`] has them,
/// and how it moves with the code.
struct LineRow {
    text: Piece,
    hint: Option<Piece>,
    background: review_ui::color::Hsla,
    pan: NotePan,
}

/// gpui's default line height, as a multiple of the font size.
pub const LINE: f32 = 1.618;

/// The height of a bar holding one line of `text_size` text with a grid step above and below
/// it: the pane headers, the find bar and the settings panel's title, search and footer.
pub fn bar_height(text_size: f32) -> f32 {
    (text_size * LINE + GRID * 2.0).round()
}

/// The height of each bottom bar: one line of `text_size` text and half a step around it.
pub fn strip_height(text_size: f32) -> f32 {
    (text_size * LINE + GRID).round()
}

/// A bare one-line text field in `text_size` text and one line tall, instead of the
/// component's fixed sizes in rems, so it fits its bar at any font size.
pub fn text_field(
    state: &gpui::Entity<gpui_component::input::InputState>,
    text_size: f32,
) -> Input {
    let line = px((text_size * LINE).round());
    Input::new(state)
        .appearance(false)
        .text_size(px(text_size))
        .line_height(line)
        .py_0()
        .h(line)
}

/// The parts of a line of code, which a row across the width and a half of a pair arrange.
pub(super) struct CellParts {
    /// The line number.
    pub gutter: Div,
    /// The `+`, `−` or blank sign.
    pub sign: Div,
    /// The code with its highlights.
    pub text: StyledText,
    /// The quiet `⏎` that stands for a blank changed line, so it never reads as an empty bar.
    pub blank: Option<Div>,
    /// The line's background, for changed lines in a style with line backgrounds.
    pub background: Option<Hsla>,
    /// The same background in review-ui's colors, for compositing what lies over it.
    pub painted_background: Option<review_ui::color::Hsla>,
    /// The text color, for changed lines in a style without syntax colors.
    pub text_color: Option<Hsla>,
}

/// A line's number in its file, with the change bar along the gutter's edge when there is one,
/// for code cells `width` pixels wide.
fn gutter(number: Piece, bar: Option<Hsla>, width: f32) -> Div {
    div()
        .w(px(super::measure::gutter(width)))
        .h_full()
        .flex_none()
        .relative()
        .flex()
        .items_center()
        .justify_end()
        .pr(px((width * 0.75).round()))
        .children(bar.map(|color| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right(px(GRID * 0.5))
                .w(px(2.0))
                .bg(color)
        }))
        .text_color(to_gpui(number.color))
        .child(number.text)
}

impl ReviewApp {
    /// The palette of the look on screen.
    pub(super) fn palette(&self) -> Palette {
        Palette::of(self)
    }

    /// A code row's gutter, where annotations put their fold arrow and before their text.
    fn gutter_width(&self) -> f32 {
        super::measure::gutter(self.sizes().code_cell())
    }

    /// A code row's gutter and sign column, which annotations indent their text past.
    fn code_columns(&self) -> f32 {
        self.gutter_width() + super::measure::sign(self.sizes().code_cell())
    }

    /// The rows of the diff list, built on demand by the virtualized list.
    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let colors = self.palette();
        let row_height = self.sizes().row_height();
        let rows: Vec<AnyElement> = range
            .filter_map(|index| {
                let row = self.rows.rows.get(index)?;
                let frame = code::frame(self, index);
                let element = match &row.body {
                    Body::Code(cell) => self
                        .code_row(index, row.file, cell, frame, &colors)
                        .h(px(row_height))
                        .into_any_element(),
                    Body::Pair(pair) => self.pair_row(index, row.file, pair, frame, &colors),
                    Body::Note(note) => self
                        .annotation_row(index, row, note, frame, &colors, cx)
                        .h(px(row_height))
                        .into_any_element(),
                };
                Some(element)
            })
            .collect();
        if let Some(bench) = &mut self.bench {
            bench.rendered_rows += rows.len();
            bench.built = bench.render_started.map(|started| started.elapsed());
        }
        rows
    }

    /// A line of code's parts, painted in `column`: the gutter, the sign, the highlighted text
    /// and the line's colors; and the text's layout, for mapping the mouse to it.
    pub(super) fn code_cell(
        &self,
        index: usize,
        file: usize,
        cell: &Cell,
        column: Column,
        colors: &Palette,
    ) -> (CellParts, super::PaintedLine) {
        let painted = code::code_cell(
            self,
            colors,
            At {
                row: index,
                file,
                column,
            },
            cell,
        );
        let (text, line) = ink::code(&cell.text, paint::merge(cell.text.len(), &painted.layers));
        let layout = super::PaintedLine {
            layout: text.layout().clone(),
            line,
        };
        let blank = painted
            .blank
            .map(|blank| div().text_color(to_gpui(blank.color)).child(blank.text));
        let code_cell = self.sizes().code_cell();
        let parts = CellParts {
            gutter: gutter(painted.number, painted.bar.map(to_gpui), code_cell),
            sign: div()
                .w(px(super::measure::sign(code_cell)))
                .flex_none()
                .text_color(to_gpui(painted.sign.color))
                .child(painted.sign.text),
            text,
            blank,
            background: painted.background.map(to_gpui),
            painted_background: painted.background,
            text_color: painted.text_color.map(to_gpui),
        };
        (parts, layout)
    }

    /// A line of code across the width: the gutter, the sign and the text, on the line's
    /// background.
    fn code_row(
        &self,
        index: usize,
        file: usize,
        cell: &Cell,
        frame: Frame,
        colors: &Palette,
    ) -> Div {
        let (parts, layout) = self.code_cell(index, file, cell, Column::Whole, colors);
        self.painted
            .borrow_mut()
            .insert((Pane::Diff, Column::Whole, index), layout);
        // The line's own background replaces its hunk's band, as the later `bg` does below.
        let behind = super::pairs::line_background(
            colors,
            &[parts.painted_background.or(frame.band(colors))],
        );
        let marker = self.diff_clip_marker(cell.width(), Column::Whole, behind, colors);
        // Exactly the list's width, however long the line: the code pans inside it, and the
        // clip marker sits at the list's right edge rather than past the line's end.
        h_flex()
            .w_full()
            .whitespace_nowrap()
            .when_some(frame.band(colors).map(to_gpui), gpui::Styled::bg)
            .when_some(parts.background, gpui::Styled::bg)
            .when_some(parts.text_color, gpui::Styled::text_color)
            .child(frame_bar(frame, colors))
            .child(parts.gutter)
            .child(parts.sign)
            .child(self.shifted(h_flex().child(parts.text).children(parts.blank), marker))
    }

    /// One of our own rows: a hunk header, a gap, a note, a move or a description, on the
    /// annotation band, in the window's font size, without a gutter.
    fn annotation_row(
        &self,
        index: usize,
        row: &Row,
        note: &Note,
        frame: Frame,
        colors: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let small = self.sizes().ui_size();
        // Exactly the list's width, as a code row is, so a row's text that runs past it is
        // clipped and marked inside the row rather than widening it past the list.
        let base = div()
            .id(("row", index))
            .w_full()
            .whitespace_nowrap()
            .text_size(px(small))
            .flex()
            .items_center();
        let shown = paint::annotation::annotation(self, colors, row, note, frame);
        let pan = paint::annotation::note_pan(self, self.reach(&shown));
        match shown {
            Annotation::Spacer => base,
            Annotation::Gap { label, rule } => base
                .cursor_pointer()
                .child(frame_bar(frame, colors))
                .child(div().w(px(self.code_columns())).flex_none())
                .child(
                    self.clipped(
                        h_flex()
                            .flex_1()
                            .child(div().w(px(GRID * 6.0)).h(px(1.0)).bg(to_gpui(rule)))
                            .child(
                                div()
                                    .px(px(GRID * 2.0))
                                    .text_color(to_gpui(label.color))
                                    .child(ink::shown(&label.text)),
                            )
                            .child(div().flex_1().h(px(1.0)).bg(to_gpui(rule))),
                    ),
                )
                .hover(|row| row.bg(to_gpui(colors.band)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.update_model(window, cx, |model| model.expand_gap(index));
                })),
            Annotation::Header(header) => self.hunk_header(header, frame, pan, base, colors),
            Annotation::Note {
                text,
                tag,
                accent,
                background,
            } => self.note_row(
                base,
                frame,
                NoteRow {
                    text,
                    tag,
                    accent,
                    background,
                    pan,
                },
                colors,
            ),
            Annotation::Line {
                text,
                hint,
                background,
            } => self.line_row(
                base,
                frame,
                LineRow {
                    text,
                    hint,
                    background,
                    pan,
                },
                colors,
            ),
        }
    }

    /// The pixels one of the tool's own rows takes from where code starts, unmoved: what its
    /// elements take, which is what moves it with the code. A hunk header's outer scopes make
    /// way first, so they count for nothing.
    fn reach(&self, shown: &Annotation) -> f32 {
        let cell = self.sizes().ui_cell();
        let cells = |piece: &Piece| scroll::float(display_width(&piece.text)) * cell;
        // Each part after the first, with the padding before it.
        let padded = |piece: Option<&Piece>| piece.map_or(0.0, |piece| GRID * 3.0 + cells(piece));
        match shown {
            Annotation::Spacer | Annotation::Gap { .. } => 0.0,
            Annotation::Header(header) => {
                let HunkHeader {
                    scope,
                    tag,
                    location,
                    chips,
                    ..
                } = header;
                // The innermost scope and the location, a chip with its padding on both
                // sides, with a gap between them in either style, then the tag after its own
                // wider gap.
                let padding = if chips.is_some() { GRID * 3.0 } else { 0.0 };
                let widths: Vec<f32> = scope
                    .as_ref()
                    .map(|scope| cells(&scope.inner))
                    .into_iter()
                    .chain(location.as_ref().map(|piece| padding + cells(piece)))
                    .collect();
                let gaps = scroll::float(widths.len().saturating_sub(1)) * GRID * 2.0;
                let tag = tag.as_ref().map_or(0.0, |tag| {
                    let gap = if widths.is_empty() {
                        0.0
                    } else {
                        self.tag_gap()
                    };
                    gap + cells(tag)
                });
                widths.iter().sum::<f32>() + gaps + tag
            }
            // The note's area starts after its accent bar, which takes the sign's place.
            Annotation::Note { text, tag, .. } => {
                let sign = measure::sign(self.sizes().code_cell());
                NOTE_ACCENT - sign + GRID * 3.0 + cells(text) + padded(tag.as_ref())
            }
            Annotation::Line { text, hint, .. } => cells(text) + padded(hint.as_ref()),
        }
    }

    /// A note's row on `base`: its accent bar, its text and its tag after it.
    fn note_row(
        &self,
        base: Stateful<Div>,
        frame: Frame,
        note: NoteRow,
        colors: &Palette,
    ) -> Stateful<Div> {
        let NoteRow {
            text,
            tag,
            accent,
            background,
            pan,
        } = note;
        let behind = super::pairs::line_background(colors, &[Some(background)]);
        base.bg(to_gpui(background))
            .child(frame_bar(frame, colors))
            .child(div().w(px(self.gutter_width())).flex_none())
            .child(
                div()
                    .w(px(NOTE_ACCENT))
                    .h_full()
                    .flex_none()
                    .bg(to_gpui(accent)),
            )
            .child(
                self.noted(
                    h_flex()
                        .child(
                            div()
                                .pl(px(GRID * 3.0))
                                .text_color(to_gpui(text.color))
                                .child(ink::shown(&text.text)),
                        )
                        .children(tag.map(|tag| {
                            div()
                                .pl(px(GRID * 3.0))
                                .text_color(to_gpui(tag.color))
                                .child(ink::shown(&tag.text))
                        })),
                    pan,
                    behind,
                    colors,
                ),
            )
    }

    /// A move's, a collapsed unit's or a description's row on `base`: its line, and what Enter
    /// does on it after it.
    fn line_row(
        &self,
        base: Stateful<Div>,
        frame: Frame,
        line: LineRow,
        colors: &Palette,
    ) -> Stateful<Div> {
        let LineRow {
            text,
            hint,
            background,
            pan,
        } = line;
        let behind = super::pairs::line_background(colors, &[Some(background)]);
        base.bg(to_gpui(background))
            .child(frame_bar(frame, colors))
            .child(div().w(px(self.code_columns())).flex_none())
            .child(
                self.noted(
                    h_flex()
                        .child(
                            div()
                                .text_color(to_gpui(text.color))
                                .child(ink::shown(&text.text)),
                        )
                        .children(hint.map(|hint| {
                            div()
                                .pl(px(GRID * 3.0))
                                .text_color(to_gpui(hint.color))
                                .child(ink::shown(&hint.text))
                        })),
                    pan,
                    behind,
                    colors,
                ),
            )
    }

    /// The space before a hunk header's tag, which sets it apart as an aside.
    fn tag_gap(&self) -> f32 {
        (self.sizes().ui_cell() * f32::from(annotation::TAG_GAP_CELLS)).round()
    }

    /// One small muted part of a hunk header with chips, such as its location.
    fn header_chip(&self, piece: &Piece, background: Hsla) -> Div {
        div()
            .flex_none()
            .px(px(GRID * 1.5))
            .line_height(px(self.sizes().ui_size() * 1.45))
            .bg(background)
            .text_color(to_gpui(piece.color))
            .child(ink::shown(&piece.text))
    }

    /// A hunk's header: one quiet line with its state glyph, the scopes around it, where it is
    /// when its code is folded away or no scope names it, and what kind of change it is.
    ///
    /// It is a gray band, blue when selected; with chips headers its location is a small muted
    /// chip. The tag is an italic aside on the band itself in either style, so the band stays
    /// whole.
    fn hunk_header(
        &self,
        header: HunkHeader,
        frame: Frame,
        pan: NotePan,
        base: Stateful<Div>,
        colors: &Palette,
    ) -> Stateful<Div> {
        let HunkHeader {
            background,
            fold,
            glyph,
            scope,
            tag,
            location,
            chips,
        } = header;
        let has_scope = scope.is_some();
        let first_tag = !has_scope && location.is_none();
        let behind = super::pairs::line_background(colors, &[Some(background)]);
        let scope = scope.map(scope_title);
        let content = if let Some(chip_background) = chips {
            h_flex()
                .min_w_0()
                .gap(px(GRID * 2.0))
                .children(scope)
                .children(location.map(|piece| self.header_chip(&piece, to_gpui(chip_background))))
        } else {
            h_flex()
                .min_w_0()
                .children(scope)
                .children(location.map(|location| {
                    div()
                        .when(has_scope, |location| location.ml(px(GRID * 2.0)))
                        .flex_none()
                        .text_color(to_gpui(location.color))
                        .child(ink::shown(&location.text))
                }))
        };
        // The tag, an aside on the band whichever style the header has.
        let tag_gap = self.tag_gap();
        let content = content.children(tag.map(|tag| {
            ink::shown_piece(tag)
                .when(!first_tag, |tag| tag.ml(px(tag_gap)))
                .flex_none()
        }));
        base.bg(to_gpui(background))
            .child(frame_bar(frame, colors))
            .child(
                div()
                    .w(px(self.gutter_width()))
                    .flex_none()
                    .pr(px((self.sizes().code_cell() * 0.75).round()))
                    .text_right()
                    .text_color(to_gpui(fold.color))
                    .child(fold.text),
            )
            .child(
                div()
                    .w(px(self.code_columns() - self.gutter_width()))
                    .flex_none()
                    .text_color(to_gpui(glyph.color))
                    .child(glyph.text),
            )
            .child(self.noted(content, pan, behind, colors))
    }

    /// The files pane: the review's progress, the filter while one narrows the tree, and the
    /// tree. The window's title names the review, so the pane has no header of its own.
    fn render_sidebar(&self, width: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let filter = files::filter_line(self, &colors);
        let [files_line, hunks_line] = files::progress_lines(
            self,
            &colors,
            Room {
                budget: width - GRID * 6.0,
                cell: self.sizes().ui_cell(),
                gap: GRID * 2.0,
            },
        );
        let count = self.visible_tree().len();
        v_flex()
            .w(px(width))
            .h_full()
            .flex_none()
            .bg(to_gpui(colors.surface))
            .child(
                v_flex()
                    .flex_none()
                    .px(px(GRID * 3.0))
                    .py(px(GRID * 2.0))
                    .gap(px(GRID))
                    .border_b_1()
                    .border_color(to_gpui(colors.hairline))
                    .child(progress_line(files_line))
                    .child(progress_line(hunks_line)),
            )
            .children(filter.map(|filter| {
                h_flex()
                    .h(px(self.sizes().bar()))
                    .flex_none()
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 2.0))
                    .whitespace_nowrap()
                    .border_b_1()
                    .border_color(to_gpui(colors.hairline))
                    .child(ink::piece(filter.query).flex_1().min_w_0().truncate())
                    .child(ink::piece(filter.count).flex_none())
            }))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(scroll::wheel_layer(cx.entity().downgrade(), List::Tree))
                    .child(
                        uniform_list(
                            "tree",
                            count,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                this.render_tree_rows(range, cx)
                            }),
                        )
                        .track_scroll(&self.lists.handles.tree)
                        .size_full(),
                    )
                    .child(Scrollbar::vertical(&self.lists.tree_bar).mode(ScrollbarMode::Always)),
            )
    }

    /// The visible tree rows in `range`, as lazygit draws its file tree: the indent, one
    /// marker cell (a folder's arrow or a file's change kind), a cell of space and the name
    /// colored by review state, with one state glyph at the right.
    fn render_tree_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let colors = self.palette();
        let row_height = self.sizes().row_height();
        let cell = self.sizes().ui_cell();
        // The marker's cell, and the space after it, which lines a folder's children up under
        // its name.
        let marker = cell.round();
        let marker_room = (cell * scroll::float(files::TREE_MARKER_CELLS)).round();
        let indent = cell * scroll::float(files::TREE_INDENT_CELLS);
        // The state column at the right.
        let column = (cell * 1.75).round();
        range
            .filter_map(|position| {
                let row = files::tree_row(self, &colors, position)?;
                let index = row.index;
                Some(
                    h_flex()
                        .id(("tree-node", index))
                        .h(px(row_height))
                        .w_full()
                        .pr(px(GRID * 3.0))
                        .pl(px(GRID * 2.0 + (scroll::float(row.depth) * indent).round()))
                        .cursor_pointer()
                        .when_some(row.background.map(to_gpui), gpui::Styled::bg)
                        .hover(|hovered| hovered.bg(to_gpui(colors.hairline)))
                        .child(
                            div().w(px(marker_room)).flex_none().child(
                                div()
                                    .w(px(marker))
                                    .text_color(to_gpui(row.marker.color))
                                    .child(SharedString::from(row.marker.text)),
                            ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(to_gpui(row.name.color))
                                .when(row.deleted, gpui::Styled::line_through)
                                .child(ink::shown(&row.name.text)),
                        )
                        .child(
                            div()
                                .w(px(column))
                                .flex_none()
                                .text_center()
                                .text_color(to_gpui(row.glyph.color))
                                .child(row.glyph.text),
                        )
                        .on_click(
                            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                let clicks = event.click_count();
                                this.update_model(window, cx, |model| {
                                    model.click_tree_row(position, index, clicks);
                                });
                            }),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// The divider between the files and the diff: a hairline with a wider grip.
    fn render_divider(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let active = self.resizing;
        let accent = to_gpui(colors.accent);
        div()
            .id("sidebar-divider")
            .group("sidebar-divider")
            .relative()
            .flex_none()
            .w(px(GRID + 1.0))
            .h_full()
            .bg(to_gpui(colors.surface))
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .child(self.regions.mark("sidebar-divider", "ew-resize"))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(1.0))
                    .bg(if active {
                        accent
                    } else {
                        to_gpui(colors.hairline)
                    })
                    .group_hover("sidebar-divider", |line| line.bg(accent)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.press_divider(event.click_count, window, cx);
                }),
            )
    }

    /// The selected file's header: its change kind, path and details, and its progress.
    fn render_file_header(&self) -> impl IntoElement + use<> {
        let colors = self.palette();
        let cell = self.sizes().ui_cell();
        let gap = GRID * 3.0;
        let room = Room {
            budget: self.diff_width - GRID * 6.0,
            cell,
            gap,
        };
        let Some(file) = header::file_header(self, &colors, room, GRID * 2.0) else {
            return h_flex()
                .h(px(self.sizes().bar()))
                .flex_none()
                .px(px(GRID * 3.0))
                .border_b_1()
                .border_color(to_gpui(colors.hairline))
                .text_color(to_gpui(colors.faint))
                .child(header::NO_FILE);
        };
        h_flex()
            .h(px(self.sizes().bar()))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(gap))
            .whitespace_nowrap()
            .bg(to_gpui(colors.surface))
            .border_b_1()
            .border_color(to_gpui(colors.hairline))
            .children(file.kind.map(ink::piece))
            .child(
                h_flex()
                    .min_w_0()
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis_start()
                            .text_color(to_gpui(file.dir.color))
                            .child(ink::shown(&file.dir.text)),
                    )
                    .child(
                        ink::shown_piece(file.name)
                            .flex_shrink_0()
                            .max_w_full()
                            .truncate(),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(to_gpui(file.details.color))
                    .child(file.details.text),
            )
            .when_some(file.counts, |line, [added, removed]| {
                line.child(ink::piece(added)).child(ink::piece(removed))
            })
            .when_some(file.meter, |line, meter| {
                line.child(progress_meter(meter, cell))
            })
    }

    /// The diff list of the selected file, with its scrollbars, or a loading or empty state.
    fn render_diff(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let row_height = self.sizes().row_height();
        // The list never scrolls sideways: rows move their code by the diff's text offset
        // instead, so gutters and the tool's own rows stay put; see `pairs`.
        let list = uniform_list(
            "rows",
            self.rows.rows.len(),
            cx.processor(|this, range, _, cx| this.render_rows(range, cx)),
        )
        .track_scroll(&self.lists.handles.diff)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::FitList)
        .size_full();
        let empty =
            header::diff_state(self, &colors, Instant::now()).map(|card| state_card(card, &colors));
        div()
            .id("list-area")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(to_gpui(colors.background))
            .text_size(px(self.code_size))
            .line_height(px(row_height))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, window, cx| {
                    this.mouse_down(Pane::Diff, event, window, cx);
                }),
            )
            .child(scroll::wheel_layer(cx.entity().downgrade(), List::Diff))
            .child(list)
            .child(Scrollbar::vertical(&self.lists.diff_bar).mode(ScrollbarMode::Always))
            .child(Scrollbar::horizontal(&self.lists.text_bar))
            .children(empty.map(|card| {
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(card)
            }))
    }

    /// The buffer: the whole file with change markers in the gutter, the cursor line, find
    /// matches and the selection.
    fn render_buffer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let row_height = self.sizes().row_height();
        let (Some(lines), Some(buffer_header)) = (
            BufferLines::of(self, colors),
            header::buffer_header(self, &colors),
        ) else {
            return div().into_any_element();
        };
        let header = self.render_buffer_header(buffer_header, &colors);
        let list = self.render_buffer_lines(lines, &colors);
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .child(
                div()
                    .id("buffer-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .bg(to_gpui(colors.background))
                    .text_size(px(self.code_size))
                    .line_height(px(row_height))
                    .cursor_text()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event, window, cx| {
                            this.mouse_down(Pane::Source, event, window, cx);
                        }),
                    )
                    .child(scroll::wheel_layer(cx.entity().downgrade(), List::Buffer))
                    .child(list)
                    .child(Scrollbar::vertical(&self.lists.buffer_bar).mode(ScrollbarMode::Always))
                    .child(Scrollbar::horizontal(&self.lists.buffer_text_bar)),
            )
            .into_any_element()
    }

    /// The buffer's lines: each line's change marker, its number, and its code panned by the
    /// buffer's own offset, with a marker where the code runs past the right edge.
    fn render_buffer_lines(
        &self,
        lines: BufferLines,
        colors: &Palette,
    ) -> impl IntoElement + use<> {
        let colors = *colors;
        let row_height = self.sizes().row_height();
        let text = Arc::clone(lines.text());
        let painted = Rc::clone(&self.painted);
        // The line numbers' column holds the last line's number, four digits at least, and a
        // cell of room.
        let code_cell = self.sizes().code_cell();
        let number_width = super::measure::buffer_numbers(code_cell, text.lines.len());
        let number_pad = code_cell.round();
        // The code moves by the buffer's own offset, past the markers and numbers that stay.
        let pan = self.buffer_text.offset();
        let offset = scroll::narrow(pan * f64::from(code_cell));
        // Before the list's first layout there is no room to measure.
        let room = Some(self.buffer_room()).filter(|&room| room > 0.0);
        let added = to_gpui(colors.added_sign);
        let removed = to_gpui(colors.removed_sign);
        uniform_list("buffer", text.lines.len(), move |range, _, _| {
            range
                .filter_map(|index| {
                    let content = text.lines.get(index)?;
                    let line = lines.line(index)?;
                    let (styled, shown) =
                        ink::code(content, paint::merge(content.len(), &line.layers));
                    painted.borrow_mut().insert(
                        (Pane::Source, Column::Whole, index),
                        super::PaintedLine {
                            layout: styled.layout().clone(),
                            line: shown,
                        },
                    );
                    let marker = room
                        .filter(|&room| code::clipped(display_width(content), pan, room))
                        .map(|_| {
                            let behind = super::pairs::line_background(&colors, &[line.background]);
                            super::pairs::clip_marker(code_cell, behind, &colors)
                        });
                    Some(
                        // Exactly the list's width, as a diff's code row is.
                        h_flex()
                            .h(px(row_height))
                            .w_full()
                            .whitespace_nowrap()
                            .when_some(line.background.map(to_gpui), gpui::Styled::bg)
                            .child(
                                div()
                                    .w(px(3.0))
                                    .h_full()
                                    .flex_none()
                                    .when(matches!(line.marker, Some(Marker::Added(_))), |bar| {
                                        bar.bg(added)
                                    })
                                    .when(matches!(line.marker, Some(Marker::Removed(_))), |bar| {
                                        bar.border_t_2().border_color(removed)
                                    }),
                            )
                            .child(
                                div()
                                    .w(px(number_width))
                                    .flex_none()
                                    .pr(px(number_pad))
                                    .text_right()
                                    .text_color(to_gpui(line.number.color))
                                    .child(line.number.text),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .overflow_hidden()
                                    .flex()
                                    .items_center()
                                    .child(h_flex().ml(px(-offset)).child(styled))
                                    .children(marker),
                            )
                            .into_any_element(),
                    )
                })
                .collect()
        })
        .track_scroll(&self.lists.handles.buffer)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::FitList)
        .size_full()
    }

    /// The buffer's header: the file and the scopes around the cursor, where the text comes
    /// from, and the cursor's place.
    fn render_buffer_header(&self, header: header::BufferHeader, colors: &Palette) -> Div {
        h_flex()
            .h(px(self.sizes().bar()))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 3.0))
            .whitespace_nowrap()
            .bg(to_gpui(colors.surface))
            .border_b_1()
            .border_color(to_gpui(colors.hairline))
            .child(
                h_flex()
                    .min_w_0()
                    .child(
                        ink::piece(header.title)
                            .flex_shrink_0()
                            .max_w_full()
                            .truncate(),
                    )
                    .children(
                        header
                            .scope
                            .map(|scope| ink::piece(scope).min_w_0().truncate()),
                    ),
            )
            .child(ink::piece(header.origin).flex_1().min_w_0().truncate())
            .child(ink::piece(header.cursor).flex_none())
    }

    /// The `/` bar of the current mode, outside the modes' key context so typing never
    /// triggers their keys: the tree's filter, or the diff's or the buffer's find.
    fn render_find_bar(&self) -> Option<impl IntoElement + use<>> {
        let colors = self.palette();
        let bar = bars::find_bar(self, &colors)?;
        let input = self.input(bar.field)?.clone();
        Some({
            h_flex()
                .key_context(Area::Find.context())
                .h(px(self.sizes().bar()))
                .flex_none()
                .px(px(GRID * 3.0))
                .gap(px(GRID * 2.0))
                .whitespace_nowrap()
                .bg(to_gpui(colors.surface))
                .border_b_1()
                .border_color(to_gpui(colors.hairline))
                .child(ink::piece(bar.slash).flex_none())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(text_field(&input, self.sizes().ui_size())),
                )
                .child(ink::piece(bar.count).flex_none())
        })
    }

    /// The hints of the bottom bar that fit the window, in rank order.
    fn bar_hints(&self, window: &Window, colors: &Palette) -> Vec<bars::BarHint> {
        let cell = self.sizes().ui_cell();
        let cells = |text: &str| scroll::float(display_width(text)) * cell;
        // The padding, the target's reserved width and the divider; a little slack covers
        // glyphs a fallback font draws wider.
        let reserved = GRID * 6.0 + GRID * 4.0 * 2.0 + 1.0 + (cell * 28.0).min(GRID * 120.0);
        let budget = (f32::from(window.viewport_size().width) - reserved) * 0.97;
        bars::bar_hints(
            self,
            colors,
            HintRoom {
                budget,
                inner_gap: GRID * 1.5,
                gap: GRID * 4.0,
            },
            cells,
        )
    }

    /// The bottom bars: the status, then which revision shows and how fresh it is at the left
    /// and the keys of this mode at the right, `? keys` last at the edge.
    fn render_bars(&self, window: &mut Window) -> impl IntoElement + use<> {
        let colors = self.palette();
        let small = self.sizes().ui_size();
        let hints = self.bar_hints(window, &colors);
        let status = bars::status(self, &colors, Instant::now());
        // What the hints leave the label: the bar's padding, the gaps and the divider apart.
        let cell = self.sizes().ui_cell();
        let cells = |text: &str| scroll::float(display_width(text)) * cell;
        let hints_width = hints
            .iter()
            .map(|hint| cells(&hint.keys.text) + GRID * 1.5 + cells(&hint.label.text))
            .sum::<f32>()
            + GRID * 4.0 * scroll::float(hints.len());
        // Three cells of slack cover glyphs a fallback font draws wider, as the hints keep some.
        let label_budget = f32::from(window.viewport_size().width)
            - GRID * 6.0
            - GRID * 4.0
            - 1.0
            - hints_width
            - cell * 3.0;
        let label = bars::sync_label(self, &colors, SystemTime::now(), label_budget, cells);
        let last = label.len().saturating_sub(1);
        v_flex()
            .flex_none()
            .text_size(px(small))
            .child(
                h_flex()
                    .h(px(self.sizes().strip()))
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 4.0))
                    .whitespace_nowrap()
                    .bg(to_gpui(status.background))
                    .border_t_1()
                    .border_color(to_gpui(colors.hairline))
                    .child(ink::piece(status.text).flex_1().min_w_0().truncate())
                    .children(status.newer.map(|newer| {
                        ink::piece(newer.text)
                            .flex_none()
                            .px(px(GRID * 2.0))
                            .bg(to_gpui(newer.background))
                    })),
            )
            .child(
                h_flex()
                    .h(px(self.sizes().strip()))
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 4.0))
                    .whitespace_nowrap()
                    .bg(to_gpui(colors.surface))
                    .child(h_flex().flex_1().min_w_0().overflow_hidden().children(
                        label.into_iter().enumerate().map(|(index, piece)| {
                            // Only the last part gives way, cut with an ellipsis.
                            if index == last {
                                ink::piece(piece).min_w_0().truncate()
                            } else {
                                ink::piece(piece).flex_none()
                            }
                        }),
                    ))
                    .child(
                        div()
                            .flex_none()
                            .w(px(1.0))
                            .h(relative(0.5))
                            .bg(to_gpui(colors.hairline)),
                    )
                    .children(hints.into_iter().map(|hint| {
                        h_flex()
                            .flex_none()
                            .gap(px(GRID * 1.5))
                            .child(ink::piece(hint.keys))
                            .child(ink::piece(hint.label))
                    })),
            )
    }

    /// The `?` cheat sheet: every bound action by section, from the keymap in effect.
    fn render_help(&self, window: &Window) -> impl IntoElement + use<> {
        let colors = self.palette();
        let small = self.sizes().ui_size();
        let cell = self.sizes().ui_cell();
        // As many columns, up to four, as leave each one its keys and room for their help.
        let (width, padding, gap) = (GRID * 360.0, GRID * 6.0, GRID * 8.0);
        let inner = width.min(f32::from(window.viewport_size().width) * 0.94) - padding * 2.0;
        let count = bars::help_column_count(inner, cell * bars::HELP_COLUMN_CELLS, gap);
        let keys_width = (cell * bars::HELP_KEYS_CELLS).round();
        let help = bars::help(self, &colors, count);
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(to_gpui(help.backdrop))
            .child(
                v_flex()
                    .id("help-sheet")
                    .track_scroll(&self.help_scroll)
                    .w(px(width))
                    .max_w(relative(0.94))
                    .max_h_full()
                    .overflow_y_scroll()
                    .p(px(padding))
                    .gap(px(GRID * 4.0))
                    .bg(to_gpui(colors.surface))
                    .border_1()
                    .border_color(to_gpui(colors.hairline))
                    .text_size(px(small))
                    .child(
                        // A narrow sheet puts the note under the title.
                        h_flex()
                            .flex_wrap()
                            .justify_between()
                            .gap_x(px(GRID * 4.0))
                            .child(ink::piece(help.title))
                            .child(ink::piece(help.note)),
                    )
                    .child(h_flex().items_start().gap(px(gap)).children(
                        help.columns.into_iter().map(|column| {
                            v_flex().flex_1().min_w_0().gap(px(GRID * 4.0)).children(
                                column.into_iter().map(|section| {
                                    v_flex()
                                        .child(
                                            ink::piece(section.title)
                                                .pb(px(GRID * 0.5))
                                                .mb(px(GRID))
                                                .truncate()
                                                .border_b_1()
                                                .border_color(to_gpui(colors.hairline)),
                                        )
                                        .children(section.entries.into_iter().map(|entry| {
                                            // Help wraps rather than hiding its end.
                                            h_flex()
                                                .items_start()
                                                .gap(px(GRID * 2.0))
                                                .child(
                                                    ink::piece(entry.keys)
                                                        .w(px(keys_width))
                                                        .flex_none()
                                                        .truncate(),
                                                )
                                                .child(ink::piece(entry.help).flex_1().min_w_0())
                                        }))
                                }),
                            )
                        }),
                    )),
            )
    }
}

/// A hunk header's scopes, such as `impl Ledger › fn apply`: the outer ones cut first when space
/// runs out.
fn scope_title(scope: Scope) -> Div {
    let Scope { outer, inner } = scope;
    h_flex()
        .min_w_0()
        .whitespace_nowrap()
        .when_some(ink::weight(inner.weight), gpui::Styled::font_weight)
        .children(outer.map(|outer| {
            div()
                .min_w_0()
                .truncate()
                .text_color(to_gpui(outer.color))
                .child(ink::shown(&outer.text))
        }))
        .child(
            div()
                .flex_none()
                .text_color(to_gpui(inner.color))
                .child(ink::shown(&inner.text)),
        )
}

/// The bar along the left edge of a hunk's rows: gray, or the accent in the selected hunk.
pub(super) fn frame_bar(frame: Frame, colors: &Palette) -> Div {
    let bar = div().w(px(FRAME)).h_full().flex_none();
    match frame.bar(colors) {
        Some(color) => bar.bg(to_gpui(color)),
        None => bar,
    }
}

/// A reviewed/total meter, for text cells `cell` pixels wide.
pub fn progress_meter(meter: Meter, cell: f32) -> Div {
    let Meter { bar, counts } = meter;
    h_flex()
        .flex_none()
        .gap(px(GRID * 2.0))
        .whitespace_nowrap()
        .child(
            div()
                .w(px((cell * METER_BAR_CELLS).round()))
                .h(px(GRID))
                .bg(to_gpui(bar.track))
                .child(
                    div()
                        .h_full()
                        .w(relative(bar.fraction))
                        .bg(to_gpui(bar.fill)),
                ),
        )
        .child(ink::piece(counts))
}

/// One labeled progress line of the files pane, with only the parts that fit.
///
/// Its text never wraps, so in the monospace font the bars of lines with equally long labels
/// line up at any font size.
fn progress_line(line: ProgressLine) -> Div {
    let ProgressLine {
        label,
        bar,
        counts,
        percent,
    } = line;
    let has_bar = bar.is_some();
    h_flex()
        .whitespace_nowrap()
        .gap(px(GRID * 2.0))
        .children(label.map(|label| ink::piece(label).flex_none()))
        .children(bar.map(|bar| {
            // The share marked by the reviewer, then the share approved automatically.
            h_flex()
                .flex_1()
                .min_w_0()
                .h(px(GRID))
                .bg(to_gpui(bar.track))
                .child(
                    div()
                        .h_full()
                        .w(relative(bar.fraction - bar.auto))
                        .bg(to_gpui(bar.fill)),
                )
                .child(
                    div()
                        .h_full()
                        .w(relative(bar.auto))
                        .bg(to_gpui(bar.auto_fill)),
                )
        }))
        .child(
            ink::piece(counts)
                .flex_none()
                // Without the bar, the counts and the percent keep to the right edge.
                .when(!has_bar, gpui::Styled::ml_auto),
        )
        .children(percent.map(|percent| ink::piece(percent).flex_none()))
}

/// A centered card for loading, empty and error states.
fn state_card(card: StateCard, colors: &Palette) -> impl IntoElement + use<> {
    v_flex()
        .max_w(px(560.0))
        .p(px(GRID * 6.0))
        .gap(px(GRID * 2.0))
        .bg(to_gpui(colors.surface))
        .border_1()
        .border_color(to_gpui(colors.hairline))
        .child(ink::piece(card.title))
        .child(ink::piece(card.detail))
}

impl ReviewApp {
    /// The frame's layout phase, before anything draws: what the scrollbars asked for, the
    /// geometry of the lists as gpui laid them out last, the model's layout in it, and the
    /// model's offsets mirrored into the lists' gpui handles.
    ///
    /// Returns whether an eased scroll runs.
    fn lay_out_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.relayout = false;
        let moves = self.take_scrollbars();
        if !moves.is_empty() {
            let Updated { effects, .. } = self.driver.update(|model| {
                for scrollbar in moves {
                    scrollbar.apply(model);
                }
            });
            self.run(effects, window, cx);
        }
        let width = self.diff_width_in(f32::from(window.viewport_size().width));
        self.follow_width(width);
        let geometry = measure::measure(&self.lists.handles, self.lists_code_size, self.diff_width);
        let now = Instant::now();
        let Updated {
            value: running,
            effects,
        } = self.driver.update(|model| model.layout(geometry, now));
        self.run(effects, window, cx);
        self.lists_code_size = self.code_size;
        self.laid_out = Some(self.grid());
        // The model worked out the side-by-side text's extent at this code cell.
        if self.list_shown(List::Diff) && self.side_by_side() {
            self.extent_cell = self.sizes().code_cell();
        }
        self.mirror_lists();
        running
    }

    /// An empty element after everything else that checks, once gpui laid the frame out, that
    /// the lists got the viewports the layout phase expected; when they did not, as after a
    /// resize, a bar coming or going, a list's first layout or a code size whose bars came out
    /// otherwise than the model worked out, the next frame lays out again.
    ///
    /// gpui lays the lists out only after the frame is built, so the layout phase can only
    /// run in the geometry of the frame before; this is the second pass for when that changed.
    fn relayout_check(cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity().downgrade();
        canvas(
            move |_, window, cx| {
                let stale = this
                    .update(cx, |this, _| {
                        let geometry = measure::measure(
                            &this.lists.handles,
                            this.lists_code_size,
                            this.diff_width,
                        );
                        this.relayout = this.laid_out != Some(this.grid_in(&geometry));
                        this.relayout
                    })
                    .unwrap_or(false);
                if stale {
                    window.request_animation_frame();
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
    }
}

impl Render for ReviewApp {
    #[expect(
        clippy::too_many_lines,
        reason = "one declarative layout with every action handler; splitting it would scatter them"
    )]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(bench) = &mut self.bench {
            bench.render_started = Some(Instant::now());
        }
        self.painted.borrow_mut().clear();
        self.regions.begin_frame();
        let running = self.lay_out_frame(window, cx);
        if running
            || matches!(self.phase, Phase::Loading(_))
            || bars::flashing(self, Instant::now())
        {
            window.request_animation_frame();
        }
        self.note_frame(window, cx);
        let colors = self.palette();
        let client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        self.client_decorated = client_decorated;
        let sidebar_width = self.shown_sidebar_width(window);
        let sidebar = self.sidebar_shown().then(|| {
            (
                self.render_sidebar(sidebar_width, cx),
                self.render_divider(cx),
            )
        });
        let right = if self.mode == Mode::Buffer && self.buffer.is_some() {
            self.render_buffer(cx).into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.render_file_header())
                .child(self.render_diff(cx))
                .into_any_element()
        };
        let bars = self.render_bars(window);
        let find_bar = self.render_find_bar();
        let overlay = match self.overlay {
            Some(Overlay::Help) => Some(self.render_help(window).into_any_element()),
            Some(Overlay::Overview) => Some(
                div()
                    .absolute()
                    .inset_0()
                    .bg(to_gpui(colors.background))
                    .child(self.render_overview(cx))
                    .into_any_element(),
            ),
            // The picker and the settings list draw apart, in their own key contexts.
            Some(Overlay::Picker | Overlay::Settings) | None => None,
        };
        let mode_context = match self.mode {
            Mode::Tree => Area::Tree,
            Mode::Diff => Area::Diff,
            Mode::Buffer => Area::Buffer,
        };
        let mode_focus = match self.mode {
            Mode::Tree => self.tree_focus.clone(),
            Mode::Diff => self.diff_focus.clone(),
            Mode::Buffer => self.buffer_focus.clone(),
        };
        let content = v_flex()
            .key_context(Area::Window.context())
            .font(self.fonts.code.clone())
            // Keys reach the key matcher here, after gpui ran the focused text field's own
            // bindings, if any.
            .capture_key_down(cx.listener(Self::key_down))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.mouse_up(window, cx)),
            )
            .size_full()
            .bg(to_gpui(colors.background))
            .text_color(to_gpui(colors.text))
            .text_size(px(self.sizes().ui_size()))
            .when(client_decorated, |content| {
                content.child(super::chrome::title_bar(
                    self.title().into(),
                    self.sizes().ui_size(),
                    window,
                    &self.regions,
                    &colors,
                    cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                        this.dispatch(review_ui::model::Action::CloseWindow, window, cx);
                    }),
                ))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .key_context(mode_context.context())
                    .track_focus(&mode_focus)
                    .children(sidebar.map(|(sidebar, divider)| {
                        h_flex().h_full().flex_none().child(sidebar).child(divider)
                    }))
                    .child(right),
            )
            .children(find_bar)
            .child(bars)
            .children(overlay.map(|overlay| {
                // The overlay owns the pointer as well as the keyboard: nothing under it reacts.
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .key_context(Area::Overlay.context())
                    .track_focus(&self.overlay_focus)
                    .child(overlay)
            }))
            .children(self.render_picker(cx).map(|picker| {
                // The picker owns the pointer and the keyboard too; its query or its list
                // takes the keys, each in its own context.
                let area = self.area();
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .key_context(area.context())
                    .track_focus(&self.overlay_focus)
                    .child(picker)
            }))
            .children(self.render_settings(window, cx))
            .child(Self::relayout_check(cx));
        window_border().child(content)
    }
}
