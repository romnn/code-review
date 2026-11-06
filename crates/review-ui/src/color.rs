//! Colors as themes store them: HSLA, with the sRGB conversions themes mix and measure in.
//!
//! The types and conversions are ported from gpui-pre 0.3.9 (`src/color.rs`) and compute the
//! same bits, so a front end that draws with gpui converts field by field without changing a
//! color; review-gui's tests check the port against gpui.

/// A color in sRGB with alpha, each channel from 0 to 1.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rgba {
    /// Red.
    pub r: f32,
    /// Green.
    pub g: f32,
    /// Blue.
    pub b: f32,
    /// Alpha: 0 is transparent, 1 opaque.
    pub a: f32,
}

/// A color as hue, saturation, lightness and alpha, each from 0 to 1.
///
/// Two colors are equal when every field is bit for bit the same, as `f32::total_cmp` compares
/// them, so equality is reflexive even for NaN.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hsla {
    /// Hue, a fraction of the full turn.
    pub h: f32,
    /// Saturation.
    pub s: f32,
    /// Lightness.
    pub l: f32,
    /// Alpha: 0 is transparent, 1 opaque.
    pub a: f32,
}

impl PartialEq for Hsla {
    fn eq(&self, other: &Self) -> bool {
        self.h
            .total_cmp(&other.h)
            .then(self.s.total_cmp(&other.s))
            .then(self.l.total_cmp(&other.l).then(self.a.total_cmp(&other.a)))
            .is_eq()
    }
}

impl Eq for Hsla {}

impl Hsla {
    /// The color in sRGB.
    #[must_use]
    pub fn to_rgb(self) -> Rgba {
        self.into()
    }

    /// The color with alpha `a`.
    #[must_use]
    pub fn alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    /// `other` laid over this color as far as `other`'s alpha reaches, keeping this color's
    /// alpha: gpui's `Hsla::blend`, which text highlights use where two of them set a color.
    #[must_use]
    pub fn blend(self, other: Self) -> Self {
        if other.a >= 1.0 {
            other
        } else if other.a <= 0.0 {
            self
        } else {
            self.to_rgb().blend(other.to_rgb()).into()
        }
    }
}

impl Rgba {
    /// `other` laid over this color as far as `other`'s alpha reaches, channel by channel in
    /// sRGB, keeping this color's alpha: gpui's `Rgba::blend`.
    #[must_use]
    pub fn blend(self, other: Self) -> Self {
        if other.a >= 1.0 {
            other
        } else if other.a <= 0.0 {
            self
        } else {
            Self {
                r: (self.r * (1.0 - other.a)) + (other.r * other.a),
                g: (self.g * (1.0 - other.a)) + (other.g * other.a),
                b: (self.b * (1.0 - other.a)) + (other.b * other.a),
                a: self.a,
            }
        }
    }

    /// The red, green and blue channels as bytes, rounded to the nearest, as an 8-bit surface
    /// stores them.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 3] {
        [self.r, self.g, self.b].map(|channel| {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the channel is clamped to 0..=1, so the rounded product fits a byte"
            )]
            let byte = (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
            byte
        })
    }
}

/// The opaque color of a `0xrrggbb` number; bits above the low 24 are ignored.
#[must_use]
pub fn rgb(hex: u32) -> Rgba {
    let [_, r, g, b] = hex.to_be_bytes().map(|byte| f32::from(byte) / 255.0);
    Rgba { r, g, b, a: 1.0 }
}

impl From<Hsla> for Rgba {
    fn from(color: Hsla) -> Self {
        let Hsla {
            h: hue,
            s: saturation,
            l: lightness,
            a: alpha,
        } = color;

        let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
        let second = chroma * (1.0 - ((hue * 6.0) % 2.0 - 1.0).abs());
        let match_value = lightness - chroma / 2.0;
        let high = chroma + match_value;
        let middle = second + match_value;
        let low = match_value;

        // The sixth of the hue circle the hue falls in; gpui's saturating cast sends NaN to 0.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the sector is floored and saturates exactly as gpui's conversion does"
        )]
        let sector = (hue * 6.0).floor() as i32;
        let (red, green, blue) = match sector {
            0 | 6 => (high, middle, low),
            1 => (middle, high, low),
            2 => (low, high, middle),
            3 => (low, middle, high),
            4 => (middle, low, high),
            _ => (high, low, middle),
        };

        Self {
            r: red.clamp(0., 1.),
            g: green.clamp(0., 1.),
            b: blue.clamp(0., 1.),
            a: alpha,
        }
    }
}

impl From<Rgba> for Hsla {
    #[expect(
        clippy::float_cmp,
        reason = "the exact comparisons are gpui's, and the port must compute the same bits"
    )]
    fn from(color: Rgba) -> Self {
        let Rgba {
            r: red,
            g: green,
            b: blue,
            a: alpha,
        } = color;

        let max = red.max(green.max(blue));
        let min = red.min(green.min(blue));
        let delta = max - min;

        #[expect(
            clippy::manual_midpoint,
            reason = "gpui computes the lightness this way, and the port must compute the same bits"
        )]
        let lightness = (max + min) / 2.0;
        let saturation = if lightness == 0.0 || lightness == 1.0 {
            0.0
        } else if lightness < 0.5 {
            delta / (2.0 * lightness)
        } else {
            delta / (2.0 - 2.0 * lightness)
        };

        let hue = if delta == 0.0 {
            0.0
        } else if max == red {
            ((green - blue) / delta).rem_euclid(6.0) / 6.0
        } else if max == green {
            ((blue - red) / delta + 2.0) / 6.0
        } else {
            ((red - green) / delta + 4.0) / 6.0
        };

        Self {
            h: hue,
            s: saturation,
            l: lightness,
            a: alpha,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure colors survive the round trip through HSLA, and grays have no saturation.
    #[test]
    fn round_trips_pure_colors() {
        for hex in [
            0x00ff_0000,
            0x0000_ff00,
            0x0000_00ff,
            0x00ff_ffff,
            0x0000_0000,
        ] {
            let color = rgb(hex);
            assert_eq!(Hsla::from(color).to_rgb(), color, "{hex:#08x}");
        }
        let gray = Hsla::from(rgb(0x0080_8080));
        assert_eq!(gray.s, 0.0);
        assert_eq!(gray.h, 0.0);
    }

    /// Blending lays the upper color over the lower one by the upper one's alpha and keeps the
    /// lower one's alpha; an opaque or transparent upper color replaces or leaves it.
    #[test]
    fn blends_by_the_upper_alpha() {
        let black = rgb(0);
        let white = rgb(0x00ff_ffff);
        let half = black.blend(Rgba { a: 0.5, ..white });
        assert_eq!(
            half,
            Rgba {
                r: 0.5,
                g: 0.5,
                b: 0.5,
                a: 1.0
            }
        );
        assert_eq!(black.blend(white), white);
        assert_eq!(black.blend(Rgba { a: 0.0, ..white }), black);
        assert_eq!(half.to_bytes(), [128, 128, 128]);
        assert_eq!(white.to_bytes(), [255, 255, 255]);
    }
}
