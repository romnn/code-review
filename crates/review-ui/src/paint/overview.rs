//! The overview's content: what the review is about (the pull request with its description, or
//! the commits), its progress and its revisions.

use review_core::ReviewInfo;

use super::header::{Meter, meter};
use super::{Palette, Piece, Run, Style, Styled, Weight};
use crate::color::Hsla;
use crate::markdown::{self, Block, Inline, Span};
use crate::model::Model;
use crate::model::overview::RevisionSummary;

/// How many characters of a commit ID show.
const COMMIT_ID_CHARS: usize = 10;

/// The overview, top to bottom.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    /// What the review is about.
    pub about: About,
    /// The `Progress` section's title.
    pub progress_title: Piece,
    /// The review's progress.
    pub progress: Meter,
    /// The `Revisions` section's title.
    pub revisions_title: Piece,
    /// The revisions, newest first.
    pub revisions: Vec<RevisionRow>,
    /// How to go back, at the end.
    pub footer: Piece,
}

/// What a review is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum About {
    /// A pull request.
    PullRequest(Box<PullRequestHead>),
    /// A local range or the working tree, by its commits.
    Commits {
        /// The review's key, semibold and large.
        title: Piece,
        /// The `Commits` section's title.
        section: Piece,
        /// The commits, newest first.
        commits: Vec<CommitRow>,
    },
    /// Nothing was fetched about it.
    Missing(Piece),
}

/// A pull request's part of the overview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestHead {
    /// `#12`, large.
    pub number: Piece,
    /// The title, semibold and large.
    pub title: Piece,
    /// `open`, `merged` or `closed`, colored by state.
    pub state: Pill,
    /// `by` the author.
    pub author: Piece,
    /// `base ← head`, in the code font.
    pub refs: Piece,
    /// The labels.
    pub labels: Vec<Pill>,
    /// When it was opened and last updated.
    pub times: Piece,
    /// The button that opens it in a browser.
    pub open: Pill,
    /// Its web page, which the button opens.
    pub url: String,
    /// The `Description` section's title.
    pub description_title: Piece,
    /// The description.
    pub description: Vec<MarkdownBlock>,
}

/// A label on a background of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    /// The label.
    pub text: Piece,
    /// The background.
    pub background: Hsla,
}

/// A commit of the reviewed range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    /// The start of its ID, in the code font.
    pub id: Piece,
    /// Its subject, semibold.
    pub subject: Piece,
    /// Its author and date.
    pub byline: Piece,
    /// The rest of its message, under the subject.
    pub body: Option<Piece>,
}

/// A revision of the review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionRow {
    /// `r2`, semibold.
    pub number: Piece,
    /// When it was recorded.
    pub time: Piece,
    /// Its head commit, in the code font.
    pub head: Piece,
    /// Its units: new, changed since reviewed, and reviewed.
    pub summary: Piece,
}

/// A block of a Markdown description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    /// A heading, semibold; levels 1 and 2 a step larger than the body.
    Heading {
        /// Whether it is a step larger.
        large: bool,
        /// Its text.
        text: Styled,
        /// Its color.
        color: Hsla,
    },
    /// A paragraph.
    Paragraph {
        /// Its text.
        text: Styled,
        /// Its color.
        color: Hsla,
    },
    /// A list item, indented by its depth.
    Item {
        /// `•`, or the item's number.
        marker: Piece,
        /// How deep it is nested, from 0.
        depth: usize,
        /// Its text.
        text: Styled,
        /// Its color.
        color: Hsla,
    },
    /// A code block, in the code font, on the editor background in a hairline frame.
    Code {
        /// Its lines.
        lines: Vec<String>,
        /// Their color.
        color: Hsla,
        /// The background.
        background: Hsla,
        /// The frame's color.
        border: Hsla,
    },
    /// A horizontal rule in this color.
    Rule(Hsla),
}

/// The overview of the open review.
#[must_use]
pub fn overview(model: &Model, palette: &Palette) -> Overview {
    let section = |title: &str| Piece::new(title, palette.dim).weighted(Weight::Semibold);
    let about = match &model.info {
        Some(ReviewInfo::PullRequest(pr)) => {
            let state_color = match pr.state.as_str() {
                "OPEN" => palette.accepted,
                "MERGED" => palette.collapsed,
                _ => palette.flagged,
            };
            About::PullRequest(Box::new(PullRequestHead {
                number: Piece::new(format!("#{}", pr.number), palette.dim),
                title: Piece::new(pr.title.clone(), palette.text).weighted(Weight::Semibold),
                state: pill(pr.state.to_lowercase(), state_color),
                author: Piece::new(format!("by {}", pr.author), palette.dim),
                refs: Piece::new(format!("{} ← {}", pr.base_ref, pr.head_ref), palette.text),
                labels: pr
                    .labels
                    .iter()
                    .map(|label| pill(label.clone(), palette.moved))
                    .collect(),
                times: Piece::new(
                    format!(
                        "opened {} · updated {}",
                        short_time(&pr.created_at),
                        short_time(&pr.updated_at)
                    ),
                    palette.faint,
                ),
                open: Pill {
                    text: Piece::new("Open in browser ↗", palette.accent),
                    background: palette.raised,
                },
                url: pr.url.clone(),
                description_title: section("Description"),
                description: markdown_blocks(&pr.body, palette),
            }))
        }
        Some(ReviewInfo::Commits { commits }) => About::Commits {
            title: Piece::new(
                model
                    .review
                    .as_ref()
                    .map_or_else(String::new, |review| review.key().to_string()),
                palette.text,
            )
            .weighted(Weight::Semibold),
            section: section("Commits"),
            commits: commits
                .iter()
                .map(|commit| CommitRow {
                    id: Piece::new(
                        commit.id.chars().take(COMMIT_ID_CHARS).collect::<String>(),
                        palette.faint,
                    ),
                    subject: Piece::new(commit.subject.clone(), palette.text)
                        .weighted(Weight::Semibold),
                    byline: Piece::new(
                        format!("{} · {}", commit.author, short_time(&commit.date)),
                        palette.faint,
                    ),
                    body: (!commit.body.is_empty())
                        .then(|| Piece::new(commit.body.clone(), palette.dim)),
                })
                .collect(),
        },
        None => About::Missing(Piece::new(
            "No pull request or commit information was fetched for this review.",
            palette.dim,
        )),
    };
    let (reviewed, total) = model.totals();
    Overview {
        about,
        progress_title: section("Progress"),
        progress: meter(palette, reviewed, total),
        revisions_title: section("Revisions"),
        revisions: model
            .revisions
            .iter()
            .rev()
            .map(|revision| revision_row(revision, palette))
            .collect(),
        footer: Piece::new("i returns to the changes", palette.faint),
    }
}

/// A pill of `label` in `color`, on a tint of it.
fn pill(label: String, color: Hsla) -> Pill {
    Pill {
        text: Piece::new(label, color),
        background: color.alpha(0.16),
    }
}

/// A revision's row.
fn revision_row(revision: &RevisionSummary, palette: &Palette) -> RevisionRow {
    RevisionRow {
        number: Piece::new(format!("r{}", revision.number), palette.text)
            .weighted(Weight::Semibold),
        time: Piece::new(format_time(revision.recorded_at), palette.dim),
        head: Piece::new(revision.head.clone(), palette.faint),
        summary: Piece::new(
            format!(
                "{} units · {} new · {} changed since reviewed · {} reviewed",
                revision.units, revision.new, revision.changed, revision.reviewed
            ),
            palette.dim,
        ),
    }
}

/// The blocks of the Markdown `source`.
fn markdown_blocks(source: &str, palette: &Palette) -> Vec<MarkdownBlock> {
    markdown::parse(source)
        .into_iter()
        .map(|block| match block {
            Block::Heading(level, text) => MarkdownBlock::Heading {
                large: level <= 2,
                text: inline(&text, palette),
                color: palette.text,
            },
            Block::Paragraph(text) => MarkdownBlock::Paragraph {
                text: inline(&text, palette),
                color: palette.text,
            },
            Block::Item {
                ordered,
                depth,
                text,
            } => MarkdownBlock::Item {
                marker: Piece::new(
                    ordered.map_or_else(|| "•".to_owned(), |number| format!("{number}.")),
                    palette.dim,
                ),
                depth,
                text: inline(&text, palette),
                color: palette.text,
            },
            Block::Code(code) => MarkdownBlock::Code {
                lines: code.lines().map(str::to_owned).collect(),
                color: palette.text,
                background: palette.background,
                border: palette.hairline,
            },
            Block::Rule => MarkdownBlock::Rule(palette.hairline),
        })
        .collect()
}

/// A Markdown inline run with its spans styled.
fn inline(text: &Inline, palette: &Palette) -> Styled {
    Styled {
        text: text.text.clone(),
        runs: text
            .spans
            .iter()
            .map(|(range, span)| Run {
                range: range.clone(),
                style: match span {
                    Span::Code => Style::block(palette.raised, palette.moved),
                    Span::Strong => Style {
                        bold: true,
                        ..Style::default()
                    },
                    Span::Emphasis => Style {
                        italic: true,
                        ..Style::default()
                    },
                    Span::Link(_) => Style {
                        underline: Some(palette.accent),
                        ..Style::text(palette.accent)
                    },
                },
            })
            .collect(),
    }
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
