//! The overview: what the review is about (the pull request or the commits), its revisions and
//! its progress.

use gpui::prelude::*;
use gpui::{
    Context, FontWeight, HighlightStyle, Hsla, SharedString, StyledText, UnderlineStyle, div, px,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{h_flex, v_flex};
use review_core::{History, ReviewInfo};

use super::ReviewApp;
use super::markdown::{self, Block, Span};
use super::render::{GRID, Palette, progress_meter};

/// One revision in the overview's history.
#[derive(Debug, Clone)]
pub struct RevisionSummary {
    /// The revision number.
    pub number: String,
    /// When it was recorded.
    pub recorded_at: u64,
    /// The head commit, abbreviated.
    pub head: String,
    /// Units in the revision.
    pub units: usize,
    /// Units never reviewed.
    pub new: usize,
    /// Units changed since they were reviewed.
    pub changed: usize,
    /// Units reviewed, carried or marked here.
    pub reviewed: usize,
}

/// Summarizes every revision of `history`.
pub fn summarize(history: &History) -> Vec<RevisionSummary> {
    history
        .revisions()
        .into_iter()
        .filter_map(|record| {
            let total = history.view(record.number).ok()?.status().total;
            Some(RevisionSummary {
                number: record.number.to_string(),
                recorded_at: record.recorded_at,
                head: record.head.as_str().chars().take(10).collect(),
                units: total.units,
                new: total.new,
                changed: total.changed,
                reviewed: total.reviewed + total.accepted + total.flagged,
            })
        })
        .collect()
}

/// Formats seconds since the Unix epoch as `YYYY-MM-DD HH:MM` in UTC.
#[must_use]
pub fn format_time(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX / 2);
    let (hour, minute) = ((seconds % 86_400) / 3600, (seconds % 3600) / 60);
    // Howard Hinnant's days-to-civil conversion.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Shortens an RFC 3339 time to `YYYY-MM-DD HH:MM`.
fn short_time(rfc3339: &str) -> String {
    rfc3339
        .get(..16)
        .map_or_else(|| rfc3339.to_owned(), |time| time.replace('T', " "))
}

/// Renders a Markdown inline run with its spans.
fn styled(text: &markdown::Inline, colors: &Palette) -> StyledText {
    let highlights = text.spans.iter().map(|(range, span)| {
        let style = match span {
            Span::Code => HighlightStyle {
                background_color: Some(colors.raised),
                color: Some(colors.moved),
                ..HighlightStyle::default()
            },
            Span::Strong => HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            },
            Span::Emphasis => HighlightStyle {
                font_style: Some(gpui::FontStyle::Italic),
                ..HighlightStyle::default()
            },
            Span::Link(_) => HighlightStyle {
                color: Some(colors.accent),
                underline: Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(colors.accent),
                    wavy: false,
                }),
                ..HighlightStyle::default()
            },
        };
        (range.clone(), style)
    });
    StyledText::new(SharedString::from(text.text.clone())).with_highlights(highlights)
}

/// Renders Markdown blocks, with headings a step above the body's `text_size`.
fn render_markdown(
    source: &str,
    text_size: f32,
    colors: &Palette,
    code_font: &gpui::Font,
) -> gpui::Div {
    let blocks = markdown::parse(source);
    v_flex()
        .gap(px(GRID * 2.0))
        .children(blocks.into_iter().map(|block| {
            match block {
                Block::Heading(level, text) => div()
                    .pt(px(GRID * 2.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .when(level <= 2, |heading| {
                        heading.text_size(px(text_size * LARGE))
                    })
                    .child(styled(&text, colors))
                    .into_any_element(),
                Block::Paragraph(text) => div()
                    .text_color(colors.text)
                    .child(styled(&text, colors))
                    .into_any_element(),
                Block::Item {
                    ordered,
                    depth,
                    text,
                } => h_flex()
                    .items_start()
                    .pl(px(GRID * 4.0 * (super::scroll::float(depth) + 1.0)))
                    .gap(px(GRID * 2.0))
                    .child(div().text_color(colors.dim).child(
                        ordered.map_or_else(|| "•".to_owned(), |number| format!("{number}.")),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(colors.text)
                            .child(styled(&text, colors)),
                    )
                    .into_any_element(),
                Block::Code(code) => div()
                    .p(px(GRID * 3.0))
                    .bg(colors.background)
                    .border_1()
                    .border_color(colors.hairline)
                    .font(code_font.clone())
                    .text_color(colors.text)
                    .children(
                        code.lines()
                            .map(|line| div().child(line.to_owned()))
                            .collect::<Vec<_>>(),
                    )
                    .into_any_element(),
                Block::Rule => div().h(px(1.0)).bg(colors.hairline).into_any_element(),
            }
        }))
}

/// A large heading's size, and a title's, as multiples of the body's.
const LARGE: f32 = 1.125;
const TITLE: f32 = 1.25;

/// A section title in the overview.
fn section(title: &'static str, colors: &Palette) -> gpui::Div {
    div()
        .pt(px(GRID * 4.0))
        .pb(px(GRID))
        .text_color(colors.dim)
        .font_weight(FontWeight::SEMIBOLD)
        .child(title)
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
        let (reviewed, total) = self.totals();
        let text_size = self.fonts.ui_size();
        let cell = self.fonts.ui_cell();
        let code_cell = self.fonts.code_cell();
        let title = px(text_size * TITLE);
        let mut body = v_flex()
            .max_w(px((cell * 100.0).round()))
            .w_full()
            .gap(px(GRID * 2.0))
            .p(px(GRID * 6.0));
        match &self.info {
            Some(ReviewInfo::PullRequest(pr)) => {
                let state_color = match pr.state.as_str() {
                    "OPEN" => colors.accepted,
                    "MERGED" => colors.collapsed,
                    _ => colors.flagged,
                };
                let url = pr.url.clone();
                body = body
                    .child(
                        h_flex()
                            .items_start()
                            .gap(px(GRID * 3.0))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(colors.dim)
                                    .text_size(title)
                                    .child(format!("#{}", pr.number)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(title)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors.text)
                                    .child(pr.title.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(px(GRID * 3.0))
                            .flex_wrap()
                            .child(pill_text(pr.state.to_lowercase(), state_color))
                            .child(
                                div()
                                    .text_color(colors.dim)
                                    .child(format!("by {}", pr.author)),
                            )
                            .child(
                                div()
                                    .font(code_font.clone())
                                    .text_color(colors.text)
                                    .child(format!("{} ← {}", pr.base_ref, pr.head_ref)),
                            )
                            .children(
                                pr.labels
                                    .iter()
                                    .map(|label| pill_text(label.clone(), colors.moved)),
                            )
                            .child(div().text_color(colors.faint).child(format!(
                                "opened {} · updated {}",
                                short_time(&pr.created_at),
                                short_time(&pr.updated_at)
                            )))
                            .child(
                                div()
                                    .id("open-in-browser")
                                    .px(px(GRID * 3.0))
                                    .py(px(GRID))
                                    .bg(colors.raised)
                                    .text_color(colors.accent)
                                    .cursor_pointer()
                                    .hover(|button| button.bg(colors.hairline))
                                    .child("Open in browser ↗")
                                    .on_click(move |_, _, cx| cx.open_url(&url)),
                            ),
                    )
                    .child(section("Description", &colors))
                    .child(render_markdown(&pr.body, text_size, &colors, &code_font));
            }
            Some(ReviewInfo::Commits { commits }) => {
                body = body
                    .child(
                        div()
                            .text_size(title)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors.text)
                            .child(
                                self.review
                                    .as_ref()
                                    .map_or_else(String::new, |review| review.key().to_string()),
                            ),
                    )
                    .child(section("Commits", &colors))
                    .children(commits.iter().map(|commit| {
                        v_flex()
                            .py(px(GRID * 1.5))
                            .gap(px(GRID))
                            .border_b_1()
                            .border_color(colors.hairline)
                            .child(
                                h_flex()
                                    .gap(px(GRID * 3.0))
                                    .child(
                                        div()
                                            .font(code_font.clone())
                                            .text_color(colors.faint)
                                            .child(commit.id.chars().take(10).collect::<String>()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_color(colors.text)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(commit.subject.clone()),
                                    )
                                    .child(div().flex_none().text_color(colors.faint).child(
                                        format!("{} · {}", commit.author, short_time(&commit.date)),
                                    )),
                            )
                            .when(!commit.body.is_empty(), |entry| {
                                entry.child(
                                    div()
                                        .pl(px((code_cell * 10.0).round() + GRID * 3.0))
                                        .text_color(colors.dim)
                                        .child(commit.body.clone()),
                                )
                            })
                    }));
            }
            None => {
                body =
                    body.child(div().text_color(colors.dim).child(
                        "No pull request or commit information was fetched for this review.",
                    ));
            }
        }
        // The revisions' columns hold their longest number and head, so they line up.
        let longest = |text: fn(&RevisionSummary) -> String| {
            self.revisions
                .iter()
                .map(|revision| text(revision).chars().count())
                .max()
                .unwrap_or(0)
        };
        let number_width = (cell
            * super::scroll::float(longest(|revision| format!("r{}", revision.number)) + 1))
        .round();
        let head_width = (code_cell
            * super::scroll::float(longest(|revision| revision.head.clone()) + 1))
        .round();
        body = body
            .child(section("Progress", &colors))
            .child(progress_meter(reviewed, total, cell, &colors))
            .child(section("Revisions", &colors))
            .children(self.revisions.iter().rev().map(|revision| {
                h_flex()
                    .gap(px(GRID * 4.0))
                    .py(px(GRID))
                    .child(
                        div()
                            .w(px(number_width))
                            .flex_none()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("r{}", revision.number)),
                    )
                    .child(
                        div()
                            .w(px((cell * 17.0).round()))
                            .flex_none()
                            .whitespace_nowrap()
                            .text_color(colors.dim)
                            .child(format_time(revision.recorded_at)),
                    )
                    .child(
                        div()
                            .w(px(head_width))
                            .flex_none()
                            .truncate()
                            .font(code_font.clone())
                            .text_color(colors.faint)
                            .child(revision.head.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(colors.dim)
                            .child(format!(
                                "{} units · {} new · {} changed since reviewed · {} reviewed",
                                revision.units, revision.new, revision.changed, revision.reviewed
                            )),
                    )
            }))
            .child(
                div()
                    .pt(px(GRID * 4.0))
                    .text_color(colors.faint)
                    .child("i returns to the changes"),
            );
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

fn pill_text(label: String, color: Hsla) -> gpui::Div {
    div()
        .px(px(GRID * 2.0))
        .bg(Hsla { a: 0.16, ..color })
        .text_color(color)
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_format_in_utc() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(946_684_800 + 3600), "2000-01-01 01:00");
        assert_eq!(format_time(1_709_251_200), "2024-03-01 00:00");
        assert_eq!(short_time("2026-01-02T03:04:05Z"), "2026-01-02 03:04");
    }
}
