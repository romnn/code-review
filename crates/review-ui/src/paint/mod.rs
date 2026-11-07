//! What the window draws and in which colors, apart from geometry, so every front end draws the
//! same review the same way.
//!
//! Each function here reads the [`Model`](crate::model::Model) and returns plain pieces: text
//! with its colors from the [`Palette`], styled runs over code, glyphs, labels, and which parts
//! make way when room runs out.
//! Where room matters, the front end passes it in its own unit with the widths its text takes:
//! logical pixels in a window, cells in a terminal.
//! How the pieces are placed (sizes, padding, bands and borders) stays with the front end.
//!
//! Colors keep their alpha.
//! A front end that blends, as gpui does, draws them as they are; a terminal composites them
//! over what is under them with [`flatten`] and [`composite`].

use std::ops::Range;

use crate::color::Hsla;

pub mod annotation;
pub mod bars;
pub mod code;
pub mod display;
pub mod files;
pub mod flatten;
pub mod header;
pub mod overview;
pub mod palette;
pub mod picker;
pub mod settings;

pub use flatten::{Flat, Ink, composite, flatten};
pub use palette::Palette;

/// How a stretch of text is styled over its line's own colors; a field left out keeps what is
/// under it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Style {
    /// The text color.
    pub color: Option<Hsla>,
    /// The color behind the text.
    pub background: Option<Hsla>,
    /// Whether the text is bold.
    pub bold: bool,
    /// Whether the text is italic.
    pub italic: bool,
    /// The color of a line under the text, one pixel thick in a window.
    pub underline: Option<Hsla>,
}

impl Style {
    /// Text in `color`.
    #[must_use]
    pub fn text(color: Hsla) -> Self {
        Self {
            color: Some(color),
            ..Self::default()
        }
    }

    /// `color` behind the text.
    #[must_use]
    pub fn behind(color: Hsla) -> Self {
        Self {
            background: Some(color),
            ..Self::default()
        }
    }

    /// A solid block, such as the cursor or the current match: `color` behind text in `text`,
    /// so the text stays readable whatever its syntax color.
    #[must_use]
    pub fn block(color: Hsla, text: Hsla) -> Self {
        Self {
            color: Some(text),
            background: Some(color),
            ..Self::default()
        }
    }

    /// `above` laid over this style, as gpui's `HighlightStyle::highlight` combines two: the
    /// upper color blends over the lower by its alpha, and the upper one's other fields win
    /// where it sets them.
    #[must_use]
    pub fn under(self, above: Self) -> Self {
        Self {
            color: above
                .color
                .map(|color| self.color.map_or(color, |below| below.blend(color)))
                .or(self.color),
            background: above.background.or(self.background),
            bold: above.bold || self.bold,
            italic: above.italic || self.italic,
            underline: above.underline.or(self.underline),
        }
    }
}

/// A styled stretch of a line, in bytes of its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The bytes it covers.
    pub range: Range<usize>,
    /// How they are styled.
    pub style: Style,
}

/// One layer of styled runs over a line, sorted, none overlapping another.
pub type Layer = Vec<Run>;

/// Flattens layered runs over a line `len` bytes long into sorted, non-overlapping runs; later
/// layers lie over earlier ones (see [`Style::under`]).
///
/// Each layer's runs are sorted and do not overlap, so one sweep over the cut points, with one
/// cursor per layer, merges them in linear time.
#[must_use]
pub fn merge(len: usize, layers: &[Layer]) -> Vec<Run> {
    let mut cuts: Vec<usize> = layers
        .iter()
        .flatten()
        .flat_map(|run| [run.range.start.min(len), run.range.end.min(len)])
        .collect();
    if cuts.is_empty() {
        return Vec::new();
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut cursors = vec![0_usize; layers.len()];
    let mut merged: Vec<Run> = Vec::new();
    for pair in cuts.windows(2) {
        let &[start, end] = pair else {
            continue;
        };
        let mut style: Option<Style> = None;
        for (layer, cursor) in layers.iter().zip(cursors.iter_mut()) {
            while layer.get(*cursor).is_some_and(|run| run.range.end <= start) {
                *cursor += 1;
            }
            if let Some(run) = layer.get(*cursor)
                && run.range.start <= start
                && end <= run.range.end
            {
                style = Some(style.map_or(run.style, |below| below.under(run.style)));
            }
        }
        if let Some(style) = style {
            match merged.last_mut() {
                Some(last) if last.range.end == start && last.style == style => {
                    last.range.end = end;
                }
                _ => merged.push(Run {
                    range: start..end,
                    style,
                }),
            }
        }
    }
    merged
}

/// How heavy a piece of text is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Weight {
    /// The body's weight.
    #[default]
    Regular,
    /// Titles: a little heavier than the body.
    Semibold,
    /// Names that lead their line, such as a file's.
    Bold,
}

/// A piece of text in one color.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// The text.
    pub text: String,
    /// Its color.
    pub color: Hsla,
    /// How heavy it is.
    pub weight: Weight,
    /// Whether it is italic, as an aside about what is beside it; a terminal that cannot show
    /// italics shows it upright.
    pub italic: bool,
}

impl Piece {
    /// `text` in `color`, upright at the body's weight.
    #[must_use]
    pub fn new(text: impl Into<String>, color: Hsla) -> Self {
        Self {
            text: text.into(),
            color,
            weight: Weight::Regular,
            italic: false,
        }
    }

    /// The piece at `weight`.
    #[must_use]
    pub fn weighted(self, weight: Weight) -> Self {
        Self { weight, ..self }
    }

    /// The piece in italics.
    #[must_use]
    pub fn italicized(self) -> Self {
        Self {
            italic: true,
            ..self
        }
    }
}

/// Text with styled runs over it, such as matches drawn in the accent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Styled {
    /// The text.
    pub text: String,
    /// Its runs, sorted and not overlapping, each on character boundaries.
    pub runs: Vec<Run>,
}

/// A progress bar: a track and the share done filled in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bar {
    /// The share done, from 0 to 1.
    pub fraction: f32,
    /// The filled part's color: the accent, or the approved color once all is done.
    pub fill: Hsla,
    /// The part of [`Self::fraction`] approved automatically, from 0 to it, drawn at the end
    /// of the filled part in [`Self::auto_fill`].
    pub auto: f32,
    /// The color of the part approved automatically, quieter than the fill.
    pub auto_fill: Hsla,
    /// The track's color.
    pub track: Hsla,
}

/// The room a line of parts has, and how wide its text and gaps are, in the front end's unit:
/// logical pixels in a window, cells in a terminal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Room {
    /// The width the parts may take.
    pub budget: f32,
    /// The width of one cell of the text.
    pub cell: f32,
    /// The space between two parts.
    pub gap: f32,
}

/// Splits a path into its directory, with the final `/`, and its file name.
#[must_use]
pub fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(slash) => path.split_at(slash + 1),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::color::rgb;

    fn red() -> Hsla {
        rgb(0x00ff_0000).into()
    }

    fn blue() -> Hsla {
        rgb(0x0000_00ff).into()
    }

    fn run(range: Range<usize>, style: Style) -> Run {
        Run { range, style }
    }

    /// Overlapping layers split into runs, and the later layer wins where both set a field.
    #[test]
    fn highlight_layers_merge_into_runs() {
        let merged = merge(
            10,
            &[
                vec![run(0..6, Style::text(red()))],
                vec![run(3..8, Style::behind(blue()))],
            ],
        );
        let ranges: Vec<Range<usize>> = merged.iter().map(|run| run.range.clone()).collect();
        assert_eq!(ranges, [0..3, 3..6, 6..8]);
        let Some(both) = merged.get(1) else {
            panic!("three runs: {merged:?}");
        };
        assert_eq!(both.style.color, Some(red()));
        assert_eq!(both.style.background, Some(blue()));
    }

    /// An upper color with alpha blends over a lower one, and a background replaces the one
    /// under it.
    #[test]
    fn upper_styles_blend_their_color_and_replace_their_background() {
        let upper = Style::block(blue().alpha(0.5), blue().alpha(0.5));
        let merged = Style::block(red(), red()).under(upper);
        assert_eq!(merged.color, Some(red().blend(blue().alpha(0.5))));
        assert_eq!(merged.background, Some(blue().alpha(0.5)));
    }

    /// Many runs merge in one pass: a long minified line stays linear.
    #[test]
    fn many_runs_merge_quickly() {
        let runs: Layer = (0..50_000)
            .map(|index| run(index * 2..index * 2 + 1, Style::text(red())))
            .collect();
        let started = Instant::now();
        let merged = merge(
            100_000,
            &[runs, vec![run(0..100_000, Style::behind(red()))]],
        );
        assert_eq!(merged.len(), 100_000);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn paths_split_into_directory_and_name() {
        assert_eq!(split_path("src/app/mod.rs"), ("src/app/", "mod.rs"));
        assert_eq!(split_path("README.md"), ("", "README.md"));
    }
}
