//! The terminal's colors: 24-bit where the terminal says it shows them, else the nearest of
//! xterm's 256.
//!
//! Every color reaching the terminal is opaque: translucent ones are composited over what is
//! under them first (see [`review_ui::paint::composite`]), as a window blends them.

use ratatui::style::Color;
use review_ui::color::{Hsla, Rgba};
use review_ui::paint::composite;

/// How the terminal shows colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colors {
    /// 24-bit color.
    TrueColor,
    /// The 256 colors of xterm's palette; the review uses its color cube and gray ramp, never
    /// the first sixteen, which each terminal's own theme redefines.
    Indexed,
}

impl Colors {
    /// The colors a terminal shows, from its `COLORTERM`: 24-bit when that says `truecolor` or
    /// `24bit`.
    #[must_use]
    pub fn from_colorterm(colorterm: Option<&str>) -> Self {
        match colorterm.map(str::to_ascii_lowercase).as_deref() {
            Some("truecolor" | "24bit") => Self::TrueColor,
            _ => Self::Indexed,
        }
    }

    /// The terminal's color for the opaque `color`.
    #[must_use]
    pub fn of(self, color: Rgba) -> Color {
        let [red, green, blue] = color.to_bytes();
        match self {
            Self::TrueColor => Color::Rgb(red, green, blue),
            Self::Indexed => Color::Indexed(nearest_indexed([red, green, blue])),
        }
    }
}

/// `color` laid over the opaque `below`, opaque.
#[must_use]
pub fn over(below: Rgba, color: Hsla) -> Rgba {
    composite(below, color)
}

/// The channel levels of xterm's 6×6×6 color cube.
const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The xterm color, from 16 to 255, nearest to `rgb`: the nearest point of the color cube or
/// of the gray ramp, whichever is closer.
fn nearest_indexed(rgb: [u8; 3]) -> u8 {
    let level = |channel: u8| {
        CUBE.iter()
            .enumerate()
            .min_by_key(|&(_, &level)| channel.abs_diff(level))
            .map_or(0, |(index, _)| index)
    };
    let [red, green, blue] = rgb;
    let (r, g, b) = (level(red), level(green), level(blue));
    let cube_rgb = [r, g, b].map(|index| CUBE.get(index).copied().unwrap_or(0));
    #[expect(
        clippy::cast_possible_truncation,
        reason = "indices of the six levels, so the sum stays below 216"
    )]
    let cube = 16 + (36 * r + 6 * g + b) as u8;

    // The gray ramp runs from 8 to 238 in steps of 10, as colors 232 to 255.
    let average = (u16::from(red) + u16::from(green) + u16::from(blue)) / 3;
    let step = (average.saturating_sub(3) / 10).min(23);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a step of the ramp, at most 23, and its level, at most 238"
    )]
    let (gray, gray_level) = (232 + step as u8, (8 + step * 10) as u8);

    let distance = |other: [u8; 3]| -> u32 {
        rgb.iter()
            .zip(other)
            .map(|(&channel, level)| u32::from(channel.abs_diff(level)).pow(2))
            .sum()
    };
    if distance([gray_level; 3]) < distance(cube_rgb) {
        gray
    } else {
        cube
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only `truecolor` and `24bit` mean 24-bit color; anything else gets the 256 colors.
    #[test]
    fn colorterm_decides() {
        assert_eq!(Colors::from_colorterm(Some("truecolor")), Colors::TrueColor);
        assert_eq!(Colors::from_colorterm(Some("24bit")), Colors::TrueColor);
        assert_eq!(Colors::from_colorterm(Some("TrueColor")), Colors::TrueColor);
        assert_eq!(Colors::from_colorterm(Some("yes")), Colors::Indexed);
        assert_eq!(Colors::from_colorterm(None), Colors::Indexed);
    }

    /// Colors on the cube map to their cube entry, grays to the ramp, and nothing maps into
    /// the first sixteen.
    #[test]
    fn indexed_colors_are_the_nearest() {
        assert_eq!(nearest_indexed([0, 0, 0]), 16);
        assert_eq!(nearest_indexed([255, 255, 255]), 231);
        assert_eq!(nearest_indexed([255, 0, 0]), 196);
        assert_eq!(nearest_indexed([95, 135, 175]), 16 + 36 + 2 * 6 + 3);
        assert_eq!(nearest_indexed([128, 128, 128]), 244);
        assert_eq!(nearest_indexed([0x16, 0x16, 0x16]), 233);
        for value in 0..=255 {
            assert!(nearest_indexed([value, value / 2, 255 - value]) >= 16);
        }
    }
}
