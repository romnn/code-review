//! The gpui forms of [`review_ui::paint`]'s pieces: text styles, weights and pieces of text.
//!
//! Code, and other text from the review such as paths and notes, shows as review-ui's
//! [`DisplayLine`] shows it, as review-tui draws it: control characters, bidi controls and
//! invisible characters as visible placeholders, which gpui's text shaping would otherwise
//! reorder text around or draw as nothing.

use std::ops::Range;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    Div, FontStyle, FontWeight, HighlightStyle, SharedString, StyledText, UnderlineStyle, div, px,
};
use review_ui::paint::display::DisplayLine;
use review_ui::paint::{self, Piece, Run, Style, Weight};

use crate::color::to_gpui;

/// The gpui weight of a piece's weight; `None` keeps the weight around it.
pub fn weight(weight: Weight) -> Option<FontWeight> {
    match weight {
        Weight::Regular => None,
        Weight::Semibold => Some(FontWeight::SEMIBOLD),
        Weight::Bold => Some(FontWeight::BOLD),
    }
}

/// The highlight style of a run's style, field for field.
pub fn highlight(style: &Style) -> HighlightStyle {
    let Style {
        color,
        background,
        bold,
        italic,
        underline,
    } = *style;
    HighlightStyle {
        color: color.map(to_gpui),
        background_color: background.map(to_gpui),
        font_weight: bold.then_some(FontWeight::BOLD),
        font_style: italic.then_some(FontStyle::Italic),
        underline: underline.map(|color| UnderlineStyle {
            thickness: px(1.0),
            color: Some(to_gpui(color)),
            wavy: false,
        }),
        ..HighlightStyle::default()
    }
}

/// The highlights of `runs`, for [`StyledText::with_highlights`].
pub fn highlights(runs: Vec<Run>) -> Vec<(Range<usize>, HighlightStyle)> {
    runs.into_iter()
        .map(|run| {
            let style = highlight(&run.style);
            (run.range, style)
        })
        .collect()
}

/// `styled`'s text with its runs.
pub fn styled(styled: paint::Styled) -> StyledText {
    let paint::Styled { text, runs } = styled;
    StyledText::new(text).with_highlights(highlights(runs))
}

/// A line of code `text` as the review shows it, with `runs` over its bytes, and the line that
/// maps the laid-out text back to them.
pub fn code(text: &str, runs: Vec<Run>) -> (StyledText, Rc<DisplayLine>) {
    let line = DisplayLine::new(text);
    let runs = line.shown_runs(runs);
    let styled = StyledText::new(line.text().to_owned()).with_highlights(highlights(runs));
    (styled, Rc::new(line))
}

/// `styled`, a line of code or a path from the review, as the review shows it.
pub fn shown_styled(styled: paint::Styled) -> StyledText {
    let paint::Styled { text, runs } = styled;
    let (styled, _) = code(&text, runs);
    styled
}

/// `text`, one line from the review such as a note or a path, as the review shows it.
pub fn shown(text: &str) -> SharedString {
    DisplayLine::new(text).text().to_owned().into()
}

/// A div holding `piece`, text from the review such as a path, as the review shows it, in its
/// color and weight.
pub fn shown_piece(piece: Piece) -> Div {
    let Piece {
        text,
        color,
        weight,
        italic,
    } = piece;
    self::piece(Piece {
        text: DisplayLine::new(&text).text().to_owned(),
        color,
        weight,
        italic,
    })
}

/// A div holding `piece` in its color, weight and slant.
pub fn piece(piece: Piece) -> Div {
    let Piece {
        text,
        color,
        weight: piece_weight,
        italic,
    } = piece;
    div()
        .text_color(to_gpui(color))
        .when_some(weight(piece_weight), gpui::Styled::font_weight)
        .when(italic, gpui::Styled::italic)
        .child(text)
}

#[cfg(test)]
mod tests {
    use review_ui::config::Background;
    use review_ui::paint::{Layer, Palette, merge};
    use review_ui::theme;

    use super::*;

    /// gpui's way of flattening layered highlights, as the window flattened them before the
    /// runs were merged in `review_ui::paint`: the same sweep, combining styles with gpui's own
    /// `HighlightStyle::highlight`.
    fn gpui_merge(
        len: usize,
        layers: &[Vec<(Range<usize>, HighlightStyle)>],
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut cuts: Vec<usize> = layers
            .iter()
            .flatten()
            .flat_map(|(range, _)| [range.start.min(len), range.end.min(len)])
            .collect();
        if cuts.is_empty() {
            return Vec::new();
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut cursors = vec![0_usize; layers.len()];
        let mut merged: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for pair in cuts.windows(2) {
            let &[start, end] = pair else {
                continue;
            };
            let mut style: Option<HighlightStyle> = None;
            for (layer, cursor) in layers.iter().zip(cursors.iter_mut()) {
                while layer
                    .get(*cursor)
                    .is_some_and(|(range, _)| range.end <= start)
                {
                    *cursor += 1;
                }
                if let Some((range, layered)) = layer.get(*cursor)
                    && range.start <= start
                    && end <= range.end
                {
                    style = Some(style.map_or(*layered, |style| style.highlight(*layered)));
                }
            }
            if let Some(style) = style {
                match merged.last_mut() {
                    Some((range, last)) if range.end == start && *last == style => {
                        range.end = end;
                    }
                    _ => merged.push((start..end, style)),
                }
            }
        }
        merged
    }

    fn run(range: Range<usize>, style: Style) -> Run {
        Run { range, style }
    }

    /// Merging the shared runs and mapping them gives exactly the highlights gpui's own
    /// merge gives for the mapped layers, in every theme: syntax colors, translucent and
    /// solid find matches, a translucent selection, underlined tokens and a translucent text
    /// color over an opaque one, overlapping every way.
    #[test]
    fn shared_runs_map_to_gpuis_highlights() {
        let (themes, _) = theme::all(None);
        for theme in &themes {
            let palette = Palette::new(theme, None, None, Background::Muted);
            let layers: Vec<Layer> = vec![
                vec![
                    run(0..4, Style::text(palette.accepted)),
                    run(4..9, Style::text(palette.moved)),
                    run(12..20, Style::text(palette.flagged)),
                ],
                vec![
                    run(2..6, Style::behind(palette.find)),
                    run(8..14, Style::block(palette.accent, palette.background)),
                ],
                vec![run(3..16, Style::behind(palette.selection))],
                vec![
                    run(
                        5..11,
                        Style {
                            underline: Some(palette.text.alpha(0.5)),
                            bold: true,
                            ..Style::default()
                        },
                    ),
                    run(13..18, Style::text(palette.text.alpha(0.6))),
                ],
            ];
            let gpui_layers: Vec<Vec<(Range<usize>, HighlightStyle)>> = layers
                .iter()
                .map(|layer| {
                    layer
                        .iter()
                        .map(|run| (run.range.clone(), highlight(&run.style)))
                        .collect()
                })
                .collect();
            assert_eq!(
                highlights(merge(20, &layers)),
                gpui_merge(20, &gpui_layers),
                "{}",
                theme.name
            );
        }
    }
}
