//! Opaque colors for a front end that cannot blend, such as a terminal: every translucent
//! color composited over what is under it, as a window would show it.
//!
//! gpui draws each color as it comes, translucent ones included, and the GPU lays it over what
//! is already drawn: gpui-pre-wgpu 0.3.9 renders to a non-sRGB (`Unorm`) surface with straight
//! alpha blending, `out = src × src.a + dst × (1 − src.a)`, and its shaders turn HSLA into
//! sRGB-encoded channels without linearizing them (`hsla_to_rgba` in `shaders.wgsl`).
//! Blending therefore happens on the encoded sRGB channels, not in linear light, and so does
//! [`composite`], with the formula of gpui's `Rgba::blend`.
//! The surface stores each blended layer in 8 bits, while [`composite`] keeps full precision
//! until [`Rgba::to_bytes`], so a stack of translucent layers can come out one step apart.
//! A run's background is drawn over the line's, and its text over that.

use std::ops::Range;

use super::{Layer, Style, merge};
use crate::color::{Hsla, Rgba};

/// A line's own colors, under any runs: its text color and its opaque background.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ink {
    /// The text color where no run sets one.
    pub foreground: Hsla,
    /// The opaque background the line is drawn on, such as a band or a changed line's.
    pub background: Rgba,
}

/// A stretch of a line in opaque colors.
#[derive(Debug, Clone, PartialEq)]
pub struct Flat {
    /// The bytes it covers.
    pub range: Range<usize>,
    /// The text color, composited over the background.
    pub foreground: Rgba,
    /// The background, composited over the line's.
    pub background: Rgba,
    /// Whether the text is bold.
    pub bold: bool,
    /// Whether the text is italic.
    pub italic: bool,
    /// The underline's color, composited over the background, when the text is underlined.
    pub underline: Option<Rgba>,
}

/// `above` laid over the opaque `below` by `above`'s alpha, in encoded sRGB as gpui's renderer
/// blends; the result is opaque.
#[must_use]
pub fn composite(below: Rgba, above: Hsla) -> Rgba {
    Rgba {
        a: 1.0,
        ..below.blend(above.to_rgb())
    }
}

/// A line `len` bytes long in opaque colors: its `layers` merged as [`merge`] merges them, over
/// `ink`, covering every byte from the first to the last in order.
///
/// A terminal colors the cells [`DisplayLine::cells`] gives each stretch's bytes.
///
/// [`DisplayLine::cells`]: super::display::DisplayLine::cells
#[must_use]
pub fn flatten(len: usize, layers: &[Layer], ink: Ink) -> Vec<Flat> {
    let plain = |range: Range<usize>| styled(range, Style::default(), ink);
    let mut flat = Vec::new();
    let mut at = 0;
    for run in merge(len, layers) {
        if run.range.start > at {
            flat.push(plain(at..run.range.start));
        }
        at = run.range.end;
        flat.push(styled(run.range, run.style, ink));
    }
    if at < len {
        flat.push(plain(at..len));
    }
    flat
}

/// `range` in `style` over `ink`.
fn styled(range: Range<usize>, style: Style, ink: Ink) -> Flat {
    let background = style
        .background
        .map_or(ink.background, |color| composite(ink.background, color));
    // A run's color blends over the line's text color, as gpui applies a highlight to the
    // text style it draws in.
    let foreground = style
        .color
        .map_or(ink.foreground, |color| ink.foreground.blend(color));
    Flat {
        range,
        foreground: composite(background, foreground),
        background,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline.map(|color| composite(background, color)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::rgb;
    use crate::paint::Run;

    fn gray(level: u32) -> Rgba {
        rgb(level * 0x0001_0101)
    }

    /// A translucent selection lies over the line's background, and the text over the result;
    /// stretches without runs keep the line's own colors.
    #[test]
    fn translucent_layers_composite_over_the_line() {
        let ink = Ink {
            foreground: gray(0xff).into(),
            background: gray(0),
        };
        let selection = Run {
            range: 2..4,
            style: Style::behind(Hsla::from(gray(0xff)).alpha(0.5)),
        };
        let flat = flatten(6, &[vec![selection]], ink);
        let ranges: Vec<Range<usize>> = flat.iter().map(|flat| flat.range.clone()).collect();
        assert_eq!(ranges, [0..2, 2..4, 4..6]);
        let backgrounds: Vec<[u8; 3]> =
            flat.iter().map(|flat| flat.background.to_bytes()).collect();
        assert_eq!(backgrounds, [[0; 3], [128; 3], [0; 3]]);
        assert!(flat.iter().all(|flat| flat.foreground == gray(0xff)));
    }

    /// Translucent text, such as a removed line's dimmed number, is mixed into its background,
    /// and the later of two backgrounds replaces the earlier rather than stacking on it.
    #[test]
    fn translucent_text_mixes_into_its_background() {
        let ink = Ink {
            foreground: Hsla::from(gray(0xff)).alpha(0.5),
            background: gray(0),
        };
        let find = Run {
            range: 0..3,
            style: Style::behind(Hsla::from(gray(0xff)).alpha(0.5)),
        };
        let selection = Run {
            range: 0..3,
            style: Style::behind(Hsla::from(gray(0xff)).alpha(0.25)),
        };
        let flat = flatten(3, &[vec![find], vec![selection]], ink);
        let [only] = flat.as_slice() else {
            panic!("one stretch: {flat:?}");
        };
        // The selection's quarter of white over black, not the find's half under it.
        assert_eq!(only.background.to_bytes(), [64; 3]);
        // Half-white text over that background.
        let expected = 0.25 + 0.75 * 0.5;
        assert_eq!(
            only.foreground,
            Rgba {
                r: expected,
                g: expected,
                b: expected,
                a: 1.0
            }
        );
    }

    /// An empty line has no stretches, and a line without runs is one stretch of its own ink.
    #[test]
    fn lines_without_runs_keep_their_ink() {
        let ink = Ink {
            foreground: gray(0xff).into(),
            background: gray(0x20),
        };
        assert_eq!(flatten(0, &[], ink), Vec::<Flat>::new());
        let flat = flatten(5, &[Vec::new()], ink);
        let [only] = flat.as_slice() else {
            panic!("one stretch: {flat:?}");
        };
        assert_eq!(only.range, 0..5);
        assert_eq!(only.background, gray(0x20));
    }
}
