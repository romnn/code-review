//! Theme colors as gpui's: [`review_ui::color::Hsla`] has gpui's fields and computes gpui's
//! bits, so converting copies the fields and changes no color.

use review_ui::color::Hsla;

/// The gpui color of a theme color.
#[must_use]
pub fn to_gpui(color: Hsla) -> gpui::Hsla {
    let Hsla { h, s, l, a } = color;
    gpui::Hsla { h, s, l, a }
}

#[cfg(test)]
mod tests {
    use review_ui::color::{Rgba, rgb};
    use review_ui::theme;

    use super::*;

    /// The theme color of a gpui color, for comparing the port's arithmetic with gpui's.
    fn from_gpui(color: gpui::Hsla) -> Hsla {
        let gpui::Hsla { h, s, l, a } = color;
        Hsla { h, s, l, a }
    }

    /// The four channels of a color, as bits, so equal means bit-identical even for NaN.
    fn bits(channels: [f32; 4]) -> [u32; 4] {
        channels.map(f32::to_bits)
    }

    fn hsla_bits(color: gpui::Hsla) -> [u32; 4] {
        bits([color.h, color.s, color.l, color.a])
    }

    fn rgba_bits(color: gpui::Rgba) -> [u32; 4] {
        bits([color.r, color.g, color.b, color.a])
    }

    fn app_rgba(color: Rgba) -> gpui::Rgba {
        let Rgba { r, g, b, a } = color;
        gpui::Rgba { r, g, b, a }
    }

    /// gpui's `mix` and `legible` as the theme computed them on gpui's colors before the port.
    fn gpui_mix(from: gpui::Hsla, to: gpui::Hsla, share: f32) -> gpui::Hsla {
        let (from, to) = (from.to_rgb(), to.to_rgb());
        let channel = |a: f32, b: f32| a + (b - a) * share;
        gpui::Rgba {
            r: channel(from.r, to.r),
            g: channel(from.g, to.g),
            b: channel(from.b, to.b),
            a: from.a,
        }
        .into()
    }

    fn gpui_luminance(color: gpui::Hsla) -> f32 {
        let rgb = color.to_rgb();
        let linear = |channel: f32| {
            if channel <= 0.040_45 {
                channel / 12.92
            } else {
                ((channel + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
    }

    fn gpui_contrast(one: gpui::Hsla, other: gpui::Hsla) -> f32 {
        let (one, other) = (gpui_luminance(one), gpui_luminance(other));
        (one.max(other) + 0.05) / (one.min(other) + 0.05)
    }

    fn gpui_legible(color: gpui::Hsla, background: gpui::Hsla, text: gpui::Hsla) -> gpui::Hsla {
        let mut shown = color;
        for step in 1..=4_u8 {
            if gpui_contrast(shown, background) >= 3.0 {
                break;
            }
            shown = gpui_mix(color, text, f32::from(step) / 4.0);
        }
        shown
    }

    /// Every color of every built-in theme, then a grid over sRGB, then HSLA corner cases:
    /// hues on and past the sector edges, grays, black, white and partial alpha.
    fn spread() -> Vec<gpui::Hsla> {
        let (themes, _) = theme::all(None);
        let mut colors: Vec<gpui::Hsla> = Vec::new();
        for theme in &themes {
            let theme::Ui {
                background,
                panel,
                band,
                raised,
                hairline,
                text,
                dim,
                faint,
                accent,
                selection,
                find,
                error,
            } = theme.ui;
            let theme::Review {
                open,
                done,
                flagged,
                changed,
            } = theme.review;
            let theme::Kind {
                added,
                modified,
                deleted,
                renamed,
                moved,
                type_change,
                quiet,
            } = theme.kind;
            let theme::Diff {
                removed_line,
                added_line,
                quiet_line,
                removed_text,
                added_text,
                removed_sign,
                added_sign,
            } = theme.diff;
            colors.extend(
                [
                    background,
                    panel,
                    band,
                    raised,
                    hairline,
                    text,
                    dim,
                    faint,
                    accent,
                    selection,
                    find,
                    error,
                    open,
                    done,
                    flagged,
                    changed,
                    added,
                    modified,
                    deleted,
                    renamed,
                    moved,
                    type_change,
                    quiet,
                    removed_line,
                    added_line,
                    quiet_line,
                    removed_text,
                    added_text,
                    removed_sign,
                    added_sign,
                ]
                .map(to_gpui),
            );
            colors.extend(theme.syntax.values().copied().map(to_gpui));
        }
        for red in (0..=0xff_u32).step_by(0x11) {
            for green in (0..=0xff_u32).step_by(0x11) {
                for blue in (0..=0xff_u32).step_by(0x11) {
                    colors.push(gpui::rgb((red << 16) | (green << 8) | blue).into());
                }
            }
        }
        for h in [0.0, 1.0 / 6.0, 0.25, 0.5, 2.0 / 3.0, 0.999, 1.0, 1.25] {
            for s in [0.0, 0.3, 1.0] {
                for l in [0.0, 0.2, 0.5, 0.8, 1.0] {
                    for a in [0.0, 0.45, 1.0] {
                        colors.push(gpui::Hsla { h, s, l, a });
                    }
                }
            }
        }
        colors
    }

    /// The port converts `#rrggbb` numbers, sRGB to HSLA and HSLA to sRGB to exactly gpui's
    /// bits, for every theme color and a spread of others.
    #[test]
    fn conversions_match_gpui() {
        for hex in (0..=0x00ff_ffff_u32).step_by(0x0001_0203) {
            assert_eq!(
                rgba_bits(app_rgba(rgb(hex))),
                rgba_bits(gpui::rgb(hex)),
                "rgb {hex:#08x}"
            );
            let expected: gpui::Hsla = gpui::rgb(hex).into();
            let ported: Hsla = rgb(hex).into();
            assert_eq!(
                hsla_bits(to_gpui(ported)),
                hsla_bits(expected),
                "rgb to hsla {hex:#08x}"
            );
        }
        for color in spread() {
            let expected = color.to_rgb();
            let ported = from_gpui(color).to_rgb();
            assert_eq!(
                rgba_bits(app_rgba(ported)),
                rgba_bits(expected),
                "hsla to rgba {color:?}"
            );
            let back: gpui::Hsla = expected.into();
            let ported_back: Hsla = ported.into();
            assert_eq!(
                hsla_bits(to_gpui(ported_back)),
                hsla_bits(back),
                "rgba to hsla {expected:?}"
            );
        }
    }

    /// The theme's `mix`, `contrast` and `legible` give gpui's bits for pairs of the spread.
    #[test]
    fn mixing_and_legibility_match_gpui() {
        let colors = spread();
        // Every color against a stride of the rest keeps the test fast but broad.
        let others: Vec<gpui::Hsla> = colors.iter().copied().step_by(29).collect();
        let (themes, _) = theme::all(None);
        let texts: Vec<gpui::Hsla> = themes
            .iter()
            .map(|theme| to_gpui(theme.ui.text))
            .chain([gpui::rgb(0x00ff_ffff).into(), gpui::rgb(0).into()])
            .collect();
        for &one in &colors {
            for &other in &others {
                for share in [0.0, 0.25, 0.5, 0.6, 1.0] {
                    assert_eq!(
                        hsla_bits(to_gpui(theme::mix(from_gpui(one), from_gpui(other), share))),
                        hsla_bits(gpui_mix(one, other, share)),
                        "mix {one:?} {other:?} {share}"
                    );
                }
                assert_eq!(
                    theme::contrast(from_gpui(one), from_gpui(other)).to_bits(),
                    gpui_contrast(one, other).to_bits(),
                    "contrast {one:?} {other:?}"
                );
            }
        }
        for &color in &others {
            for &background in &others {
                for &text in &texts {
                    assert_eq!(
                        hsla_bits(to_gpui(theme::legible(
                            from_gpui(color),
                            from_gpui(background),
                            from_gpui(text)
                        ))),
                        hsla_bits(gpui_legible(color, background, text)),
                        "legible {color:?} on {background:?} toward {text:?}"
                    );
                }
            }
        }
    }

    /// The port blends one color over another to exactly gpui's bits, as text highlights
    /// combine their colors, for colors of the spread under translucent and opaque ones.
    #[test]
    fn blending_matches_gpui() {
        let colors = spread();
        let below: Vec<gpui::Hsla> = colors.iter().copied().step_by(7).collect();
        let above: Vec<gpui::Hsla> = colors
            .iter()
            .copied()
            .step_by(29)
            .flat_map(|color| [0.0, 0.25, 0.45, 0.6, 1.0].map(|a| gpui::Hsla { a, ..color }))
            .collect();
        for &one in &below {
            for &other in &above {
                assert_eq!(
                    hsla_bits(to_gpui(from_gpui(one).blend(from_gpui(other)))),
                    hsla_bits(one.blend(other)),
                    "blend {other:?} over {one:?}"
                );
            }
        }
    }
}
