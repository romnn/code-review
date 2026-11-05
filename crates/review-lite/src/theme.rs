//! Themes as data: semantic colors for the window, the review states, change kinds, the diff and
//! syntax, read from TOML files. The built-in themes live in `themes/`, each citing the palette
//! it comes from; more can be dropped into `$XDG_CONFIG_HOME/code-review/themes/`.

use std::collections::BTreeMap;
use std::path::Path;

use gpui::{Hsla, rgb};
use serde::Deserialize;

use crate::syntax::Tone;

/// The built-in themes, the default first.
const BUILT_IN: &[&str] = &[
    include_str!("../themes/carbonfox.toml"),
    include_str!("../themes/tokyonight-night.toml"),
    include_str!("../themes/nightfox.toml"),
    include_str!("../themes/nordfox.toml"),
    include_str!("../themes/terafox.toml"),
    include_str!("../themes/duskfox.toml"),
    include_str!("../themes/one-dark.toml"),
    include_str!("../themes/catppuccin-mocha.toml"),
    include_str!("../themes/rose-pine.toml"),
];

/// The default theme's name.
pub const DEFAULT: &str = "carbonfox";

/// A theme file as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: String,
    title: Option<String>,
    ui: BTreeMap<String, String>,
    review: BTreeMap<String, String>,
    kind: BTreeMap<String, String>,
    diff: BTreeMap<String, String>,
    #[serde(default)]
    syntax: BTreeMap<String, String>,
}

/// The window's colors, by role.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ui {
    /// The code's background.
    pub background: Hsla,
    /// The files pane and the bars.
    pub panel: Hsla,
    /// The surface our own annotations sit on: hunk headers, notes, gaps.
    pub band: Hsla,
    /// A selected row.
    pub raised: Hsla,
    pub hairline: Hsla,
    pub text: Hsla,
    pub dim: Hsla,
    pub faint: Hsla,
    pub accent: Hsla,
    pub selection: Hsla,
    pub find: Hsla,
    pub error: Hsla,
}

/// Review states' colors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Review {
    /// Still needs the reviewer.
    pub open: Hsla,
    /// Approved.
    pub done: Hsla,
    pub flagged: Hsla,
    /// Changed since it was reviewed.
    pub changed: Hsla,
}

/// Change kinds' colors, apart from review states.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Kind {
    pub added: Hsla,
    pub modified: Hsla,
    pub deleted: Hsla,
    pub renamed: Hsla,
    pub moved: Hsla,
    /// A type change, such as a file becoming a symlink.
    pub type_change: Hsla,
    /// Formatting-only and generated files.
    pub quiet: Hsla,
}

/// The diff's colors.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Diff {
    pub removed_line: Hsla,
    pub added_line: Hsla,
    /// Reformatted lines: present, but without emphasis.
    pub quiet_line: Hsla,
    /// Removed and added text in the plain style.
    pub removed_text: Hsla,
    pub added_text: Hsla,
    pub removed_sign: Hsla,
    pub added_sign: Hsla,
}

/// A complete theme.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub title: String,
    pub ui: Ui,
    pub review: Review,
    pub kind: Kind,
    pub diff: Diff,
    /// Syntax colors by highlight tone; a missing tone keeps the text color.
    pub syntax: BTreeMap<String, Hsla>,
}

impl Theme {
    /// Parses a theme file.
    ///
    /// # Errors
    ///
    /// Returns a description of what is wrong: bad TOML, a missing role or a bad color.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: File = toml::from_str(text).map_err(|err| err.message().to_owned())?;
        let get = |section: &BTreeMap<String, String>, table: &str, key: &str| {
            let value = section
                .get(key)
                .ok_or_else(|| format!("[{table}] {key} is missing"))?;
            color(value)
                .ok_or_else(|| format!("[{table}] {key} = {value:?} is not a #rrggbb color"))
        };
        let ui = |key| get(&file.ui, "ui", key);
        let review = |key| get(&file.review, "review", key);
        let kind = |key| get(&file.kind, "kind", key);
        let diff = |key| get(&file.diff, "diff", key);
        let mut syntax = BTreeMap::new();
        for (key, value) in &file.syntax {
            let parsed = color(value)
                .ok_or_else(|| format!("[syntax] {key} = {value:?} is not a #rrggbb color"))?;
            syntax.insert(key.clone(), parsed);
        }
        Ok(Self {
            title: file.title.clone().unwrap_or_else(|| file.name.clone()),
            name: file.name.clone(),
            ui: Ui {
                background: ui("background")?,
                panel: ui("panel")?,
                band: ui("band")?,
                raised: ui("raised")?,
                hairline: ui("hairline")?,
                text: ui("text")?,
                dim: ui("dim")?,
                faint: ui("faint")?,
                accent: ui("accent")?,
                selection: ui("selection")?,
                find: ui("find")?,
                error: ui("error")?,
            },
            review: Review {
                open: review("open")?,
                done: review("done")?,
                flagged: review("flagged")?,
                changed: review("changed")?,
            },
            kind: Kind {
                added: kind("added")?,
                modified: kind("modified")?,
                deleted: kind("deleted")?,
                renamed: kind("renamed")?,
                moved: kind("moved")?,
                type_change: kind("type")?,
                quiet: kind("quiet")?,
            },
            diff: Diff {
                removed_line: diff("removed_line")?,
                added_line: diff("added_line")?,
                quiet_line: diff("quiet_line")?,
                removed_text: diff("removed_text")?,
                added_text: diff("added_text")?,
                removed_sign: diff("removed_sign")?,
                added_sign: diff("added_sign")?,
            },
            syntax,
        })
    }

    /// The color of a syntax tone, or `None` for the text color.
    #[must_use]
    pub fn syntax(&self, tone: Tone) -> Option<Hsla> {
        let key = match tone {
            Tone::Plain => return None,
            Tone::Keyword => "keyword",
            Tone::Function => "function",
            Tone::Type => "type",
            Tone::String => "string",
            Tone::Constant => "constant",
            Tone::Comment => "comment",
            Tone::Parameter => "parameter",
            Tone::Attribute => "attribute",
            Tone::Operator => "operator",
            Tone::Punctuation => "punctuation",
            Tone::Property => "property",
            Tone::Special => "special",
            Tone::Tag => "tag",
            Tone::Escape => "escape",
        };
        self.syntax.get(key).copied()
    }
}

/// The least contrast ratio text keeps on a tinted line: WCAG's for large text, enough for
/// code that is meant to be read but may stay quieter than the rest.
const LEGIBLE: f32 = 3.0;

/// `color`, mixed toward `text` in quarter steps until it has a contrast ratio of at least 3
/// against `background`.
#[must_use]
pub fn legible(color: Hsla, background: Hsla, text: Hsla) -> Hsla {
    let mut shown = color;
    for step in 1..=4_u8 {
        if contrast(shown, background) >= LEGIBLE {
            break;
        }
        shown = mix(color, text, f32::from(step) / 4.0);
    }
    shown
}

/// `from` moved `share` of the way to `to`, channel by channel in sRGB, keeping `from`'s alpha.
#[must_use]
pub fn mix(from: Hsla, to: Hsla, share: f32) -> Hsla {
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

/// The WCAG contrast ratio of two colors, from 1 to 21.
#[must_use]
pub fn contrast(one: Hsla, other: Hsla) -> f32 {
    let (one, other) = (luminance(one), luminance(other));
    (one.max(other) + 0.05) / (one.min(other) + 0.05)
}

/// The relative luminance of a color, as WCAG defines it.
fn luminance(color: Hsla) -> f32 {
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

/// Parses a `#rrggbb` color.
fn color(text: &str) -> Option<Hsla> {
    let digits = text.strip_prefix('#')?;
    (digits.len() == 6).then_some(())?;
    u32::from_str_radix(digits, 16)
        .ok()
        .map(|hex| rgb(hex).into())
}

/// Every theme: the built-in ones, then those in `user_dir`, which replace a built-in theme of
/// the same name. Returns the problems with user themes alongside.
#[must_use]
pub fn all(user_dir: Option<&Path>) -> (Vec<Theme>, Vec<String>) {
    let mut themes: Vec<Theme> = BUILT_IN
        .iter()
        .filter_map(|text| Theme::parse(text).ok())
        .collect();
    let mut problems = Vec::new();
    let mut paths: Vec<_> = user_dir
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    paths.sort();
    for path in paths {
        let parsed = std::fs::read_to_string(&path)
            .map_err(|err| err.to_string())
            .and_then(|text| Theme::parse(&text));
        match parsed {
            Ok(theme) => match themes.iter_mut().find(|known| known.name == theme.name) {
                Some(known) => *known = theme,
                None => themes.push(theme),
            },
            Err(err) => problems.push(format!("theme {}: {err}", path.display())),
        }
    }
    (themes, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in theme parses and defines every syntax tone.
    #[test]
    fn built_in_themes_are_complete() {
        assert_eq!(BUILT_IN.len(), 9);
        for text in BUILT_IN {
            let theme = Theme::parse(text).unwrap();
            for tone in [Tone::Keyword, Tone::String, Tone::Comment, Tone::Function] {
                assert!(
                    theme.syntax(tone).is_some(),
                    "{} lacks {tone:?}",
                    theme.name
                );
            }
        }
        let (themes, problems) = all(None);
        assert_eq!(
            themes.first().map(|theme| theme.name.as_str()),
            Some(DEFAULT)
        );
        assert_eq!(problems, Vec::<String>::new());
    }

    /// Every built-in theme's syntax colors, lifted where needed, stay readable on its
    /// removed and added lines, bold or muted, and its text on its background.
    #[test]
    fn syntax_stays_readable_on_tinted_lines() {
        for text in BUILT_IN {
            let theme = Theme::parse(text).unwrap();
            assert!(
                contrast(theme.ui.text, theme.ui.background) >= 4.5,
                "{} text",
                theme.name
            );
            // Annotations are muted, never unreadable.
            for surface in [theme.ui.background, theme.ui.band, theme.ui.panel] {
                assert!(
                    contrast(theme.ui.dim, surface) >= LEGIBLE,
                    "{} dim on {surface:?}: {}",
                    theme.name,
                    contrast(theme.ui.dim, surface)
                );
            }
            // Bold lines, and muted ones as the palette mixes them.
            let muted = |line| mix(line, theme.ui.background, crate::look::MUTED_SHARE);
            for tint in [
                theme.diff.removed_line,
                theme.diff.added_line,
                muted(theme.diff.removed_line),
                muted(theme.diff.added_line),
            ] {
                for color in theme.syntax.values() {
                    let shown = legible(*color, tint, theme.ui.text);
                    assert!(
                        contrast(shown, tint) >= LEGIBLE - 0.01
                            || contrast(theme.ui.text, tint) < LEGIBLE,
                        "{}: {color:?} on {tint:?}",
                        theme.name
                    );
                }
            }
        }
    }

    /// A user theme replaces a built-in one of the same name, and a broken one is reported.
    #[test]
    fn user_themes_replace_and_report() {
        let dir = tempfile::tempdir().unwrap();
        let carbon = BUILT_IN[0].replace("#161616", "#000000");
        std::fs::write(dir.path().join("mine.toml"), carbon).unwrap();
        std::fs::write(dir.path().join("broken.toml"), "name = \"x\"\n").unwrap();
        let (themes, problems) = all(Some(dir.path()));
        assert_eq!(themes.len(), 9);
        assert_eq!(themes[0].ui.background, rgb(0x0000_0000).into());
        assert_eq!(problems.len(), 1, "{problems:?}");
    }
}
