//! The pull request overview, as review-gui draws it: what the review is about (the pull
//! request with its description, or the commits), its progress and its revisions, in a column
//! across the middle of the screen.
//!
//! It is laid out as rows of the column's width, then drawn from where it is scrolled; see
//! [`SheetScroll`].
//! The terminal cannot open a browser for the reviewer, so where review-gui has a button that
//! opens the pull request, the overview shows its address, which the terminal can follow.

use ratatui::layout::Rect;
use review_ui::color::{Hsla, Rgba};
use review_ui::model::Model;
use review_ui::paint::header::{METER_BAR_CELLS, Meter};
use review_ui::paint::overview::{About, MarkdownBlock, Pill, RevisionRow, overview};
use review_ui::paint::{Ink, Palette, Piece, Run, Style, Styled, Weight};

use super::files::meter_bar;
use super::painter::{Painter, slice, width, wrap};
use super::{SheetScroll, opaque};
use crate::color::over;

/// The widest column, in cells.
const COLUMN_CELLS: u16 = 100;

/// The cells between the screen's edges and the column.
const PAD: u16 = 3;

/// The cells between the parts of a line of parts, such as the pull request's state and its
/// author.
const GAP: u16 = 3;

/// A piece of a row of the overview, at a cell of the column.
struct Span {
    /// Its first cell, from the column's left edge.
    at: u16,
    /// Its text and runs.
    text: Styled,
    /// Its color.
    color: Hsla,
    /// Whether it is bold.
    bold: bool,
    /// Its own background, such as a pill's.
    background: Option<Hsla>,
}

/// A row of the overview.
#[derive(Default)]
struct Row {
    spans: Vec<Span>,
    /// A block's background behind the row, from a cell of the column, so many cells wide.
    band: Option<(u16, u16, Hsla)>,
    /// A meter drawn at a cell of the column.
    meter: Option<(u16, Meter)>,
}

/// The overview's rows in a column `width` cells wide.
struct Document {
    width: u16,
    rows: Vec<Row>,
}

impl Document {
    fn blank(&mut self) {
        self.rows.push(Row::default());
    }

    /// `piece` on a row of its own from cell `at`.
    fn line(&mut self, at: u16, piece: &Piece) {
        self.rows.push(Row {
            spans: vec![span(at, piece, None)],
            ..Row::default()
        });
    }

    /// `text` wrapped into the cells from `at` to the column's edge, in `color`, bold when
    /// `bold`.
    fn wrapped(&mut self, at: u16, text: &Styled, color: Hsla, bold: bool) {
        let room = self.width.saturating_sub(at).max(1);
        for range in wrap(&text.text, room) {
            self.rows.push(Row {
                spans: vec![Span {
                    at,
                    text: slice(text, range),
                    color,
                    bold,
                    background: None,
                }],
                ..Row::default()
            });
        }
    }

    /// `parts` one after another with [`GAP`] between them, onto a new row whenever the next
    /// does not fit; a part with a background has a cell of it on each side.
    fn flow(&mut self, parts: Vec<(Piece, Option<Hsla>)>) {
        let mut row = Row::default();
        let mut at = 0;
        for (piece, background) in parts {
            let text = match background {
                Some(_) => Piece {
                    text: format!(" {} ", piece.text),
                    ..piece
                },
                None => piece,
            };
            let cells = width(&text.text);
            if at > 0 && at + GAP + cells > self.width {
                self.rows.push(std::mem::take(&mut row));
                at = 0;
            }
            if at > 0 {
                at += GAP;
            }
            row.spans.push(span(at, &text, background));
            at += cells;
        }
        self.rows.push(row);
    }
}

/// A span of `piece` at `at`, bold unless the piece is regular.
fn span(at: u16, piece: &Piece, background: Option<Hsla>) -> Span {
    Span {
        at,
        text: Styled {
            text: piece.text.clone(),
            runs: Vec::new(),
        },
        color: piece.color,
        bold: piece.weight != Weight::Regular,
        background,
    }
}

/// The overview over the whole screen `screen`, scrolled as `scroll` says, which learns how
/// far it can go.
pub fn draw(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    screen: Rect,
    scroll: &mut SheetScroll,
) {
    let background = opaque(palette.background);
    painter.fill(screen, background);
    let width = COLUMN_CELLS.min(screen.width.saturating_sub(2 * PAD));
    let document = document(model, palette, width);
    let viewport = screen.height.saturating_sub(2);
    let rows = u16::try_from(document.rows.len()).unwrap_or(u16::MAX);
    scroll.max = rows.saturating_sub(viewport);
    scroll.viewport = viewport;
    scroll.offset = scroll.offset.min(scroll.max);
    let left = screen.x + screen.width.saturating_sub(width) / 2;
    let top = screen.y + 1;
    for (row, y) in document
        .rows
        .iter()
        .skip(usize::from(scroll.offset))
        .zip(top..top + viewport)
    {
        draw_row(painter, row, left, y, width, background);
    }
}

/// One row at `left` on row `y`, `width` cells wide, on `background`.
fn draw_row(painter: &mut Painter<'_>, row: &Row, left: u16, y: u16, width: u16, background: Rgba) {
    let mut bg = background;
    if let Some((at, cells, color)) = row.band {
        bg = over(background, color);
        painter.fill(Rect::new(left + at, y, cells, 1), bg);
    }
    if let Some((at, meter)) = &row.meter {
        draw_meter(painter, meter, left + *at, y, left + width, background);
    }
    for span in &row.spans {
        let x = left + span.at;
        let span_bg = span.background.map_or(bg, |color| over(bg, color));
        let mut layers = Vec::new();
        if span.bold {
            layers.push(vec![Run {
                range: 0..span.text.text.len(),
                style: Style {
                    bold: true,
                    ..Style::default()
                },
            }]);
        }
        layers.push(span.text.runs.clone());
        let ink = Ink {
            foreground: span.color,
            background: span_bg,
        };
        let room = (left + width).saturating_sub(x);
        painter.layered(x, y, room, 0, &span.text.text, &layers, ink);
    }
}

/// A meter from `x`: its bar, then its counts.
fn draw_meter(painter: &mut Painter<'_>, meter: &Meter, x: u16, y: u16, right: u16, bg: Rgba) {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a bar of a few cells"
    )]
    let cells = METER_BAR_CELLS as u16;
    meter_bar(
        painter,
        x,
        y,
        cells.min(right.saturating_sub(x)),
        &meter.bar,
        bg,
    );
    let at = x + cells + 2;
    painter.piece(at, y, right.saturating_sub(at), &meter.counts, bg);
}

/// The overview of `model` as rows of a column `width` cells wide.
fn document(model: &Model, palette: &Palette, width: u16) -> Document {
    let content = overview(model, palette);
    let mut document = Document {
        width,
        rows: Vec::new(),
    };
    match content.about {
        About::PullRequest(pr) => {
            let number_width = width_of(&pr.number);
            let first = document.rows.len();
            document.wrapped(
                number_width + 2,
                &plain(&pr.title.text),
                pr.title.color,
                true,
            );
            if let Some(row) = document.rows.get_mut(first) {
                row.spans.insert(0, span(0, &pr.number, None));
            }
            document.blank();
            let mut parts = vec![pill(pr.state), (pr.author, None), (pr.refs, None)];
            parts.extend(pr.labels.into_iter().map(pill));
            parts.push((pr.times, None));
            document.flow(parts);
            document.line(0, &Piece::new(pr.url, pr.open.text.color));
            section(&mut document, &pr.description_title);
            markdown(&mut document, pr.description);
        }
        About::Commits {
            title,
            section: commits_title,
            commits,
        } => {
            document.wrapped(0, &plain(&title.text), title.color, true);
            section(&mut document, &commits_title);
            for commit in commits {
                let id_width = width_of(&commit.id);
                let byline_width = width_of(&commit.byline);
                let subject_at = id_width + 2;
                let byline_at = width.saturating_sub(byline_width);
                let subject_room = byline_at.saturating_sub(subject_at + 2);
                let subject = Piece {
                    text: super::painter::ellipsize_end(&commit.subject.text, subject_room),
                    ..commit.subject
                };
                document.rows.push(Row {
                    spans: vec![
                        span(0, &commit.id, None),
                        span(subject_at, &subject, None),
                        span(byline_at, &commit.byline, None),
                    ],
                    ..Row::default()
                });
                if let Some(body) = commit.body {
                    for line in body.text.lines() {
                        document.wrapped(subject_at, &plain(line), body.color, false);
                    }
                }
            }
        }
        About::Missing(note) => document.wrapped(0, &plain(&note.text), note.color, false),
    }
    section(&mut document, &content.progress_title);
    document.rows.push(Row {
        meter: Some((0, content.progress)),
        ..Row::default()
    });
    section(&mut document, &content.revisions_title);
    revisions(&mut document, &content.revisions, width);
    document.blank();
    document.line(0, &content.footer);
    document
}

/// The cells `piece` takes.
fn width_of(piece: &Piece) -> u16 {
    width(&piece.text)
}

/// `text` without runs.
fn plain(text: &str) -> Styled {
    Styled {
        text: text.to_owned(),
        runs: Vec::new(),
    }
}

/// A pill's part of a line of parts.
fn pill(pill: Pill) -> (Piece, Option<Hsla>) {
    (pill.text, Some(pill.background))
}

/// A section's title, after a blank row.
fn section(document: &mut Document, title: &Piece) {
    document.blank();
    document.line(0, title);
}

/// The revisions as columns of their number, time and head, which line up, and their summary
/// wrapped in what is left.
fn revisions(document: &mut Document, revisions: &[RevisionRow], width: u16) {
    let widest = |text: fn(&RevisionRow) -> &str| {
        revisions
            .iter()
            .map(|revision| super::painter::width(text(revision)))
            .max()
            .unwrap_or(0)
    };
    let number_at = 0;
    let time_at = number_at + widest(|revision| &revision.number.text) + 2;
    let head_at = time_at + widest(|revision| &revision.time.text) + 2;
    let summary_at = head_at + widest(|revision| &revision.head.text) + 2;
    for revision in revisions {
        let first = document.rows.len();
        if summary_at < width {
            document.wrapped(
                summary_at,
                &plain(&revision.summary.text),
                revision.summary.color,
                false,
            );
        } else {
            document.blank();
        }
        if let Some(row) = document.rows.get_mut(first) {
            row.spans.insert(0, span(head_at, &revision.head, None));
            row.spans.insert(0, span(time_at, &revision.time, None));
            row.spans.insert(0, span(number_at, &revision.number, None));
        }
    }
}

/// A description's Markdown blocks, a blank row between them but for list items that follow
/// each other.
fn markdown(document: &mut Document, blocks: Vec<MarkdownBlock>) {
    let mut item_before = false;
    for block in blocks {
        let item = matches!(block, MarkdownBlock::Item { .. });
        if !(item && item_before) {
            document.blank();
        }
        item_before = item;
        match block {
            MarkdownBlock::Heading { text, color, .. } => document.wrapped(0, &text, color, true),
            MarkdownBlock::Paragraph { text, color } => document.wrapped(0, &text, color, false),
            MarkdownBlock::Item {
                marker,
                depth,
                text,
                color,
            } => {
                let indent = u16::try_from(depth + 1).unwrap_or(1).saturating_mul(2);
                let text_at = indent + width_of(&marker) + 1;
                let first = document.rows.len();
                document.wrapped(text_at, &text, color, false);
                if let Some(row) = document.rows.get_mut(first) {
                    row.spans.insert(0, span(indent, &marker, None));
                }
            }
            MarkdownBlock::Code {
                lines,
                color,
                background,
                border,
            } => code_block(document, &lines, [color, background, border]),
            MarkdownBlock::Rule(color) => {
                let rule = Piece::new("─".repeat(usize::from(document.width)), color);
                document.line(0, &rule);
            }
        }
    }
}

/// A code block: its lines on its background in a hairline frame, with a cell of air inside.
fn code_block(document: &mut Document, lines: &[String], colors: [Hsla; 3]) {
    let [color, background, border] = colors;
    let width = document.width;
    let inner = width.saturating_sub(2);
    let edge = |left: &str, right: &str| {
        Piece::new(
            format!("{left}{}{right}", "─".repeat(usize::from(inner))),
            border,
        )
    };
    document.line(0, &edge("┌", "┐"));
    for line in lines {
        let room = inner.saturating_sub(2);
        let shown = super::painter::ellipsize_end(line, room);
        document.rows.push(Row {
            spans: vec![
                span(0, &Piece::new("│", border), None),
                span(2, &Piece::new(shown, color), None),
                span(width.saturating_sub(1), &Piece::new("│", border), None),
            ],
            band: Some((1, inner, background)),
            meter: None,
        });
    }
    document.line(0, &edge("└", "┘"));
}
