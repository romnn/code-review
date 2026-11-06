//! The overview: what the review is about (the pull request or the commits), its revisions and
//! its progress.

use gpui::prelude::*;
use gpui::{Context, div, px};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{h_flex, v_flex};
use review_ui::paint::Piece;
use review_ui::paint::overview::{About, MarkdownBlock, Pill, overview};

use super::render::{GRID, progress_meter};
use super::{ReviewApp, ink};
use crate::color::to_gpui;

/// Draws a description's Markdown blocks, with large headings a step above the body's
/// `text_size`.
fn render_markdown(
    blocks: Vec<MarkdownBlock>,
    text_size: f32,
    code_font: &gpui::Font,
) -> gpui::Div {
    v_flex()
        .gap(px(GRID * 2.0))
        .children(blocks.into_iter().map(|block| {
            match block {
                MarkdownBlock::Heading { large, text, color } => div()
                    .pt(px(GRID * 2.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(to_gpui(color))
                    .when(large, |heading| heading.text_size(px(text_size * LARGE)))
                    .child(ink::styled(text))
                    .into_any_element(),
                MarkdownBlock::Paragraph { text, color } => div()
                    .text_color(to_gpui(color))
                    .child(ink::styled(text))
                    .into_any_element(),
                MarkdownBlock::Item {
                    marker,
                    depth,
                    text,
                    color,
                } => h_flex()
                    .items_start()
                    .pl(px(GRID * 4.0 * (super::scroll::float(depth) + 1.0)))
                    .gap(px(GRID * 2.0))
                    .child(ink::piece(marker))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(to_gpui(color))
                            .child(ink::styled(text)),
                    )
                    .into_any_element(),
                MarkdownBlock::Code {
                    lines,
                    color,
                    background,
                    border,
                } => div()
                    .p(px(GRID * 3.0))
                    .bg(to_gpui(background))
                    .border_1()
                    .border_color(to_gpui(border))
                    .font(code_font.clone())
                    .text_color(to_gpui(color))
                    .children(
                        lines
                            .into_iter()
                            .map(|line| div().child(line))
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element(),
                MarkdownBlock::Rule(color) => {
                    div().h(px(1.0)).bg(to_gpui(color)).into_any_element()
                }
            }
        }))
}

/// A large heading's size, and a title's, as multiples of the body's.
const LARGE: f32 = 1.125;
const TITLE: f32 = 1.25;

/// A section title in the overview.
fn section(title: Piece) -> gpui::Div {
    ink::piece(title).pt(px(GRID * 4.0)).pb(px(GRID))
}

impl ReviewApp {
    /// The overview pane.
    #[expect(
        clippy::too_many_lines,
        reason = "one declarative layout; splitting it would scatter the element tree"
    )]
    pub(super) fn render_overview(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let code_font = self.fonts.code.clone();
        let content = overview(self, &colors);
        let text_size = self.sizes().ui_size();
        let cell = self.sizes().ui_cell();
        let code_cell = self.sizes().code_cell();
        let title = px(text_size * TITLE);
        let mut body = v_flex()
            .max_w(px((cell * 100.0).round()))
            .w_full()
            .gap(px(GRID * 2.0))
            .p(px(GRID * 6.0));
        match content.about {
            About::PullRequest(pr) => {
                let pr = *pr;
                let url = pr.url;
                body = body
                    .child(
                        h_flex()
                            .items_start()
                            .gap(px(GRID * 3.0))
                            .child(ink::piece(pr.number).flex_none().text_size(title))
                            .child(
                                ink::shown_piece(pr.title)
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(title),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(px(GRID * 3.0))
                            .flex_wrap()
                            .child(pill(pr.state))
                            .child(ink::shown_piece(pr.author))
                            .child(ink::shown_piece(pr.refs).font(code_font.clone()))
                            .children(pr.labels.into_iter().map(pill))
                            .child(ink::piece(pr.times))
                            .child(
                                div()
                                    .id("open-in-browser")
                                    .px(px(GRID * 3.0))
                                    .py(px(GRID))
                                    .bg(to_gpui(pr.open.background))
                                    .text_color(to_gpui(pr.open.text.color))
                                    .cursor_pointer()
                                    .hover(|button| button.bg(to_gpui(colors.hairline)))
                                    .child(pr.open.text.text)
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            ),
                    )
                    .child(section(pr.description_title))
                    .child(render_markdown(pr.description, text_size, &code_font));
            }
            About::Commits {
                title: heading,
                section: commits_title,
                commits,
            } => {
                body = body
                    .child(ink::piece(heading).text_size(title))
                    .child(section(commits_title))
                    .children(commits.into_iter().map(|commit| {
                        v_flex()
                            .py(px(GRID * 1.5))
                            .gap(px(GRID))
                            .border_b_1()
                            .border_color(to_gpui(colors.hairline))
                            .child(
                                h_flex()
                                    .gap(px(GRID * 3.0))
                                    .child(ink::piece(commit.id).font(code_font.clone()))
                                    .child(ink::shown_piece(commit.subject).flex_1().min_w_0())
                                    .child(ink::shown_piece(commit.byline).flex_none()),
                            )
                            .children(commit.body.map(|text| {
                                ink::piece(text).pl(px((code_cell * 10.0).round() + GRID * 3.0))
                            }))
                    }));
            }
            About::Missing(note) => {
                body = body.child(ink::piece(note));
            }
        }
        // The revisions' columns hold their longest number and head, so they line up.
        let longest = |text: fn(&review_ui::paint::overview::RevisionRow) -> &str| {
            content
                .revisions
                .iter()
                .map(|revision| text(revision).chars().count())
                .max()
                .unwrap_or(0)
        };
        let number_width = (cell
            * super::scroll::float(longest(|revision| revision.number.text.as_str()) + 1))
        .round();
        let head_width = (code_cell
            * super::scroll::float(longest(|revision| revision.head.text.as_str()) + 1))
        .round();
        body = body
            .child(section(content.progress_title))
            .child(progress_meter(content.progress, cell))
            .child(section(content.revisions_title))
            .children(content.revisions.into_iter().map(|revision| {
                h_flex()
                    .gap(px(GRID * 4.0))
                    .py(px(GRID))
                    .child(
                        ink::piece(revision.number)
                            .w(px(number_width))
                            .flex_none()
                            .whitespace_nowrap(),
                    )
                    .child(
                        ink::piece(revision.time)
                            .w(px((cell * 17.0).round()))
                            .flex_none()
                            .whitespace_nowrap(),
                    )
                    .child(
                        ink::piece(revision.head)
                            .w(px(head_width))
                            .flex_none()
                            .truncate()
                            .font(code_font.clone()),
                    )
                    .child(ink::piece(revision.summary).flex_1().min_w_0())
            }))
            .child(ink::piece(content.footer).pt(px(GRID * 4.0)));
        div()
            .id("overview")
            .relative()
            .size_full()
            .child(
                div()
                    .id("overview-scroll")
                    .size_full()
                    .track_scroll(&self.overview_scroll)
                    .overflow_y_scroll()
                    .flex()
                    .justify_center()
                    // The body keeps its own height, so what is past the window scrolls.
                    .items_start()
                    .child(body),
            )
            .child(Scrollbar::vertical(&self.overview_scroll).mode(ScrollbarMode::Always))
            .on_click(cx.listener(|_, _, _, _| {}))
    }
}

/// A label on a tint of its own color.
fn pill(pill: Pill) -> gpui::Div {
    ink::piece(pill.text)
        .px(px(GRID * 2.0))
        .bg(to_gpui(pill.background))
}
