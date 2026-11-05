//! How the review looks: the theme, the diff style, the density, the layout and token
//! emphasis, and the named presets that bundle them.
//!
//! A diff style says how a diff is colored whatever the theme: syntax colors or not, what
//! changed lines are drawn on, how hunk headers are drawn, and whether changed lines have a bar
//! in the gutter. It never moves a row, so cycling the styles never shifts the content; the
//! rows' spacing is the density's alone.
//! Three styles are built in, in the order `|` cycles them (`plain` as lazygit draws a diff,
//! `bold` as delta does, `soft` as Zed does), and `[styles.<name>]` tables add more.
//!
//! The window keeps the look the reviewer applied and, while the settings list's cursor rests
//! on an entry, a preview of it; what shows is the preview when there is one.
//! A preset is resolved against the loaded themes and styles as a whole before any of it shows,
//! so a preset naming a theme or a style that does not exist is reported, never applied with a
//! fallback.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::config::{
    Background, Density, DiffLayout, Headers, PresetConfig, Remembered, StyleConfig,
    points_to_pixels,
};
use crate::theme::{self, Theme};

/// How a diff is colored, whatever the theme; nothing in it moves a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffStyle {
    /// The name `|`, the settings list and the configuration use.
    pub name: String,
    /// Where it comes from.
    pub origin: Origin,
    /// Whether the diff shows syntax colors; without them, changed lines are red and green
    /// text.
    pub syntax: bool,
    /// What changed lines are drawn on.
    pub background: Background,
    /// How hunk headers are drawn.
    pub headers: Headers,
    /// Whether changed lines have a colored bar in the gutter.
    pub change_bar: bool,
}

impl DiffStyle {
    /// A one-line summary, such as `syntax colors · muted line backgrounds · chips · change
    /// bar`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = vec![
            if self.syntax {
                "syntax colors"
            } else {
                "red/green text"
            }
            .to_owned(),
            match self.background {
                Background::None => "no line backgrounds".to_owned(),
                other => format!("{} line backgrounds", other.name()),
            },
            format!("{} headers", self.headers.name()),
        ];
        if self.change_bar {
            parts.push("change bar".to_owned());
        }
        parts.join(" · ")
    }
}

/// The name of the style the diff has by default: delta's look.
pub const DEFAULT_STYLE: &str = "bold";

/// The default style, whose fields a user style leaves out.
fn bold() -> DiffStyle {
    DiffStyle {
        name: DEFAULT_STYLE.to_owned(),
        origin: Origin::BuiltIn,
        syntax: true,
        background: Background::Bold,
        headers: Headers::Bands,
        change_bar: false,
    }
}

/// The built-in diff styles, in the order `|` cycles them.
fn built_in_styles() -> Vec<DiffStyle> {
    vec![
        DiffStyle {
            name: "plain".to_owned(),
            origin: Origin::BuiltIn,
            syntax: false,
            background: Background::None,
            headers: Headers::Bands,
            change_bar: false,
        },
        bold(),
        DiffStyle {
            name: "soft".to_owned(),
            origin: Origin::BuiltIn,
            syntax: true,
            background: Background::Muted,
            headers: Headers::Chips,
            change_bar: true,
        },
    ]
}

/// Every diff style: the built-in ones, then the user's in name order; a user style of a
/// built-in one's name takes its place.
///
/// Fields a user style leaves out take the bold style's.
#[must_use]
pub fn styles(user: &BTreeMap<String, StyleConfig>) -> Vec<DiffStyle> {
    let mut styles = built_in_styles();
    let base = bold();
    for (name, config) in user {
        let style = DiffStyle {
            name: name.clone(),
            origin: Origin::User,
            syntax: config.syntax.unwrap_or(base.syntax),
            background: config.background.unwrap_or(base.background),
            headers: config.headers.unwrap_or(base.headers),
            change_bar: config.change_bar.unwrap_or(base.change_bar),
        };
        match styles.iter_mut().find(|known| known.name == *name) {
            Some(known) => *known = style,
            None => styles.push(style),
        }
    }
    styles
}

/// The style `name` names among `styles`; `syntax`, the name the configuration used before
/// styles were bundles, is the bold style.
///
/// # Errors
///
/// Returns why there is no such style.
pub fn find_style(styles: &[DiffStyle], name: &str) -> Result<DiffStyle, String> {
    let name = if name == "syntax" {
        DEFAULT_STYLE
    } else {
        name
    };
    styles
        .iter()
        .find(|style| style.name == name)
        .cloned()
        .ok_or_else(|| format!("there is no diff style {name:?}"))
}

/// The style after `current` in `styles`, wrapping around: what `|` switches to.
#[must_use]
pub fn next_style<'a>(styles: &'a [DiffStyle], current: &str) -> Option<&'a DiffStyle> {
    let at = styles.iter().position(|style| style.name == current);
    let next = at.map_or(0, |at| (at + 1) % styles.len().max(1));
    styles.get(next)
}

/// Everything a preset sets: the look of the diff apart from the font.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    /// The theme.
    pub theme: Arc<Theme>,
    /// How the diff is colored.
    pub style: DiffStyle,
    /// How much air the rows have.
    pub density: Density,
    /// The layout the reviewer asked for; a narrow window may still show the diff unified.
    pub layout: DiffLayout,
    /// Whether changed tokens within a line are underlined.
    pub token_emphasis: bool,
}

impl Look {
    /// A one-line summary, such as `One Dark · soft · comfortable · unified`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = vec![
            self.theme.title.clone(),
            self.style.name.clone(),
            self.density.name().to_owned(),
            self.layout.name().to_owned(),
        ];
        if self.token_emphasis {
            parts.push("token emphasis".to_owned());
        }
        parts.join(" · ")
    }

    /// The settings that remember this whole look, written together.
    #[must_use]
    pub fn remembered(&self) -> Vec<Remembered> {
        vec![
            Remembered {
                table: "theme",
                key: "name",
                value: self.theme.name.as_str().into(),
            },
            Remembered {
                table: "diff",
                key: "style",
                value: self.style.name.as_str().into(),
            },
            Remembered {
                table: "diff",
                key: "density",
                value: self.density.name().into(),
            },
            Remembered {
                table: "diff",
                key: "layout",
                value: self.layout.name().into(),
            },
            Remembered {
                table: "diff",
                key: "token_emphasis",
                value: self.token_emphasis.into(),
            },
        ]
    }
}

/// The look the reviewer applied, and the one previewed over it.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    /// What the reviewer chose last; applying moves it, and only it is remembered.
    pub applied: Look,
    /// What the settings list shows while its cursor rests on an entry that changes the look.
    pub preview: Option<Look>,
}

impl Appearance {
    /// The look on screen: the preview, or else the applied look.
    #[must_use]
    pub fn shown(&self) -> &Look {
        self.preview.as_ref().unwrap_or(&self.applied)
    }
}

/// The line height of a density, as a multiple of the code size.
#[must_use]
pub fn line_height(density: Density) -> f32 {
    match density {
        Density::Compact => 1.5,
        Density::Normal => 1.6,
        Density::Comfortable => 1.75,
    }
}

/// How far muted line backgrounds move from the theme's toward the editor background.
pub const MUTED_SHARE: f32 = 0.5;

/// Where a style or a preset comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Shipped with the app.
    BuiltIn,
    /// A table of the configuration.
    User,
}

/// A named look, resolved against the loaded themes and styles.
#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    /// The name the settings list and the configuration use.
    pub name: String,
    /// Where it comes from.
    pub origin: Origin,
    /// The complete look, or why the preset cannot be used.
    pub look: Result<Look, String>,
}

/// The built-in presets, in the order the settings list shows them.
fn built_in_presets() -> Vec<(&'static str, PresetConfig)> {
    let carbonfox = Some(theme::DEFAULT.to_owned());
    let preset = |theme: &Option<String>, style: &str, density, layout| PresetConfig {
        theme: theme.clone(),
        style: Some(style.to_owned()),
        density: Some(density),
        layout: Some(layout),
        token_emphasis: Some(false),
    };
    vec![
        (
            "delta",
            preset(&carbonfox, "bold", Density::Normal, DiffLayout::Unified),
        ),
        (
            "delta side-by-side",
            preset(&carbonfox, "bold", Density::Normal, DiffLayout::SideBySide),
        ),
        (
            "lazygit",
            preset(&carbonfox, "plain", Density::Compact, DiffLayout::Unified),
        ),
        // Zed's own default theme with its quieter colors and roomier rows.
        (
            "zed",
            preset(
                &Some("one-dark".to_owned()),
                "soft",
                Density::Comfortable,
                DiffLayout::Unified,
            ),
        ),
    ]
}

/// Resolves `preset` against `themes` and `styles` into a complete look; fields it leaves out
/// take their defaults.
///
/// # Errors
///
/// Returns why the preset cannot be used: it names a theme or a style that is not loaded.
pub fn resolve(
    preset: &PresetConfig,
    themes: &[Theme],
    styles: &[DiffStyle],
) -> Result<Look, String> {
    let wanted = preset.theme.as_deref().unwrap_or(theme::DEFAULT);
    let theme = themes
        .iter()
        .find(|theme| theme.name == wanted)
        .ok_or_else(|| format!("there is no theme {wanted:?}"))?;
    let style = find_style(styles, preset.style.as_deref().unwrap_or(DEFAULT_STYLE))?;
    Ok(Look {
        theme: Arc::new(theme.clone()),
        style,
        density: preset.density.unwrap_or_default(),
        layout: preset.layout.unwrap_or_default(),
        token_emphasis: preset.token_emphasis.unwrap_or(false),
    })
}

/// Every preset: the built-in ones, then the user's in name order; a user preset of a built-in
/// one's name takes its place.
#[must_use]
pub fn presets(
    user: &BTreeMap<String, Result<PresetConfig, String>>,
    themes: &[Theme],
    styles: &[DiffStyle],
) -> Vec<Preset> {
    let mut presets: Vec<Preset> = built_in_presets()
        .into_iter()
        .map(|(name, preset)| Preset {
            name: name.to_owned(),
            origin: Origin::BuiltIn,
            look: resolve(&preset, themes, styles),
        })
        .collect();
    for (name, preset) in user {
        let preset = Preset {
            name: name.clone(),
            origin: Origin::User,
            look: preset
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|preset| resolve(preset, themes, styles)),
        };
        match presets.iter_mut().find(|known| known.name == *name) {
            Some(known) => *known = preset,
            None => presets.push(preset),
        }
    }
    presets
}

/// The font sizes the settings list offers, in points.
pub const FONT_SIZES: &[u8] = &[10, 11, 12, 13, 14, 15, 16, 18, 20];

/// The size in points of `pixels` logical pixels.
#[must_use]
pub fn points_of(pixels: f32) -> f32 {
    pixels / points_to_pixels(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|` cycles plain, bold and soft, then the user's styles, and back; a user style takes
    /// the bold style's fields it leaves out, and replaces a built-in style of its name.
    #[test]
    fn styles_cycle_and_user_styles_fill_in_from_bold() {
        let user = BTreeMap::from([
            (
                "calm".to_owned(),
                StyleConfig {
                    background: Some(Background::Muted),
                    ..StyleConfig::default()
                },
            ),
            (
                "soft".to_owned(),
                StyleConfig {
                    change_bar: Some(false),
                    ..StyleConfig::default()
                },
            ),
        ]);
        let styles = styles(&user);
        let names: Vec<&str> = styles.iter().map(|style| style.name.as_str()).collect();
        assert_eq!(names, ["plain", "bold", "soft", "calm"]);
        let calm = &styles[3];
        assert!(calm.syntax);
        assert_eq!(calm.background, Background::Muted);
        assert_eq!(calm.headers, Headers::Bands);
        let soft = &styles[2];
        assert_eq!(soft.origin, Origin::User);
        assert!(!soft.change_bar);
        assert_eq!(
            soft.headers,
            Headers::Bands,
            "taken from bold, not the old soft"
        );

        let cycle: Vec<&str> = ["plain", "bold", "soft", "calm", "missing"]
            .iter()
            .map(|name| next_style(&styles, name).unwrap().name.as_str())
            .collect();
        assert_eq!(cycle, ["bold", "soft", "calm", "plain", "plain"]);
        assert_eq!(find_style(&styles, "syntax").unwrap().name, "bold");
        assert!(find_style(&styles, "loud").is_err());
    }

    /// The built-in presets resolve with the built-in themes and styles, "zed" is the soft one,
    /// and a user preset replaces a built-in one of its name.
    #[test]
    fn presets_resolve_and_user_presets_replace_built_in_ones() {
        let (themes, _) = theme::all(None);
        let styles = styles(&BTreeMap::new());
        let presets = presets(&BTreeMap::new(), &themes, &styles);
        let names: Vec<&str> = presets.iter().map(|preset| preset.name.as_str()).collect();
        assert_eq!(names, ["delta", "delta side-by-side", "lazygit", "zed"]);
        assert!(presets.iter().all(|preset| preset.look.is_ok()));
        let zed = presets[3].look.as_ref().unwrap();
        assert_eq!(zed.style.name, "soft");
        assert_eq!(zed.theme.name, "one-dark");
        assert_eq!(zed.density, Density::Comfortable);
        let lazygit = presets[2].look.as_ref().unwrap();
        assert_eq!(lazygit.style.name, "plain");
        assert_eq!(lazygit.density, Density::Compact);
        assert_eq!(presets[0].look.as_ref().unwrap().density, Density::Normal);

        let user = BTreeMap::from([
            (
                "zed".to_owned(),
                Ok(PresetConfig {
                    theme: Some("nordfox".to_owned()),
                    ..PresetConfig::default()
                }),
            ),
            (
                "mine".to_owned(),
                Ok(PresetConfig {
                    layout: Some(DiffLayout::SideBySide),
                    ..PresetConfig::default()
                }),
            ),
        ]);
        let presets = super::presets(&user, &themes, &styles);
        assert_eq!(presets.len(), 5);
        let zed = presets.iter().find(|preset| preset.name == "zed").unwrap();
        assert_eq!(zed.origin, Origin::User);
        let look = zed.look.as_ref().unwrap();
        assert_eq!(look.theme.name, "nordfox");
        // Fields a preset leaves out take their defaults, not the built-in preset's.
        assert_eq!(look.style.name, DEFAULT_STYLE);
        assert_eq!(look.density, Density::Normal);
        let mine = presets.last().unwrap();
        assert_eq!(mine.look.as_ref().unwrap().theme.name, theme::DEFAULT);
    }

    /// A preset naming a missing theme or style is an error as a whole, never a fallback with
    /// the rest of the preset applied.
    #[test]
    fn unknown_themes_and_styles_make_the_preset_unusable() {
        let (themes, _) = theme::all(None);
        let styles = styles(&BTreeMap::new());
        let preset = PresetConfig {
            theme: Some("solarized".to_owned()),
            style: Some("soft".to_owned()),
            ..PresetConfig::default()
        };
        assert_eq!(
            resolve(&preset, &themes, &styles),
            Err("there is no theme \"solarized\"".to_owned())
        );
        let preset = PresetConfig {
            style: Some("loud".to_owned()),
            ..PresetConfig::default()
        };
        assert_eq!(
            resolve(&preset, &themes, &styles),
            Err("there is no diff style \"loud\"".to_owned())
        );
    }

    /// A preset is remembered as all of its fields together.
    #[test]
    fn a_look_remembers_every_field() {
        let (themes, _) = theme::all(None);
        let look = resolve(&PresetConfig::default(), &themes, &styles(&BTreeMap::new())).unwrap();
        let keys: Vec<(&str, &str)> = look
            .remembered()
            .iter()
            .map(|setting| (setting.table, setting.key))
            .collect();
        assert_eq!(
            keys,
            [
                ("theme", "name"),
                ("diff", "style"),
                ("diff", "density"),
                ("diff", "layout"),
                ("diff", "token_emphasis"),
            ]
        );
    }
}
