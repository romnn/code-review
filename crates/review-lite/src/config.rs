//! The user's settings for review-lite, read from `$XDG_CONFIG_HOME/code-review/config.toml`.
//!
//! ```toml
//! [font]
//! family = "SFMono Nerd Font"
//! size = 13        # points, as terminals count them
//! weight = "Medium" # a name or a number from 100 to 900
//!
//! [view]
//! context = 3       # unchanged lines around each hunk
//!
//! [diff]
//! style = "bold"    # plain, bold, soft or one of [styles]; `|` cycles them
//! density = "normal"    # or "compact" or "comfortable": the rows' spacing
//! layout = "unified"    # or "side-by-side"; `s` toggles it
//! approved = "folded"   # or "shown": approved hunks show their code; `zA` toggles it
//! removed = "#5f1519"   # overrides the theme's line backgrounds
//! added = "#14471f"
//! token_emphasis = false
//!
//! [styles.calm]     # a diff style beside plain, bold and soft: colors only, never spacing
//! syntax = true         # syntax colors in the diff
//! background = "muted"  # changed lines' background: "none", "muted" or "bold"
//! headers = "chips"     # hunk headers as "bands" across the width or quiet "chips"
//! change_bar = true     # a colored bar in the gutter of changed lines
//!
//! [theme]
//! name = "carbonfox"  # or tokyonight-night, nightfox, one-dark, …; T opens the settings
//!
//! [presets.mine]    # a named look the settings list offers beside the built-in ones
//! theme = "nordfox"     # fields left out take their defaults
//! style = "soft"
//! density = "comfortable"
//! layout = "side-by-side"
//! token_emphasis = false
//!
//! [keys]            # replace an action's keys; `?` in the app lists the actions
//! NextUnit = ["n", "ctrl-j"]
//! ToggleOverview = []   # unbound
//!
//! [layout]          # remembered when the panes are resized or the sidebar toggled
//! sidebar = 300     # logical pixels
//! source = 600      # the definition pane; absent for 42% of the window
//! sidebar_visible = true
//! ```
//!
//! Choices made in the app are written back with [`remember`], which edits the file as TOML and
//! keeps everything else in it: comments, presets, keys and settings it does not change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Points per inch, and the pixels per inch that a logical pixel assumes: gpui's logical pixels
/// are 1/96 inch before the display scale factor, which gpui applies itself.
const POINTS_PER_INCH: f32 = 72.0;
const PIXELS_PER_INCH: f32 = 96.0;

/// The default code size in points, matching a 13-point terminal font.
pub const DEFAULT_SIZE_POINTS: f32 = 13.0;

/// The default code font weight: Medium.
pub const DEFAULT_WEIGHT: f32 = 500.0;

/// Why the configuration could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file exists but cannot be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file is not valid TOML or has values of the wrong type.
    #[error("invalid {path}: {message}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

/// The code font settings; `None` fields use the defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FontConfig {
    /// The family to try first.
    pub family: Option<String>,
    /// The size in points.
    pub size_points: Option<f32>,
    /// The weight, from 100 to 900.
    pub weight: Option<f32>,
}

impl FontConfig {
    /// The size in gpui's logical pixels.
    #[must_use]
    pub fn size_pixels(&self) -> f32 {
        points_to_pixels(self.size_points.unwrap_or(DEFAULT_SIZE_POINTS))
    }
}

/// Converts points to logical pixels.
#[must_use]
pub fn points_to_pixels(points: f32) -> f32 {
    points * PIXELS_PER_INCH / POINTS_PER_INCH
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    font: FontSection,
    #[serde(default)]
    view: ViewSection,
    #[serde(default)]
    diff: DiffSection,
    #[serde(default)]
    layout: LayoutSection,
    #[serde(default)]
    keys: BTreeMap<String, KeyList>,
    #[serde(default)]
    theme: ThemeSection,
    /// Read table by table, so a mistake in one style or preset leaves the others usable.
    #[serde(default)]
    styles: BTreeMap<String, toml::Value>,
    #[serde(default)]
    presets: BTreeMap<String, toml::Value>,
}

/// One key or several, as `[keys]` accepts them.
#[derive(Deserialize)]
#[serde(untagged)]
enum KeyList {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LayoutSection {
    sidebar: Option<f32>,
    source: Option<f32>,
    sidebar_visible: Option<bool>,
}

/// The sidebar's default width in logical pixels.
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 300.0;

/// The pane widths and whether the sidebar shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutConfig {
    /// The sidebar's width in logical pixels.
    pub sidebar: f32,
    /// The definition pane's width in logical pixels; `None` gives it 42% of the window.
    pub source: Option<f32>,
    /// Whether the sidebar shows.
    pub sidebar_visible: bool,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            sidebar: DEFAULT_SIDEBAR_WIDTH,
            source: None,
            sidebar_visible: true,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ViewSection {
    context: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ThemeSection {
    name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DiffSection {
    style: Option<String>,
    density: Option<Density>,
    layout: Option<DiffLayout>,
    approved: Option<Approved>,
    removed: Option<String>,
    added: Option<String>,
    token_emphasis: Option<bool>,
}

/// A `[styles.<name>]` table as written; every field is optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StyleSection {
    syntax: Option<bool>,
    background: Option<Background>,
    headers: Option<Headers>,
    change_bar: Option<bool>,
    /// Read only to say where it belongs now: a style never changes the spacing.
    density: Option<serde::de::IgnoredAny>,
}

/// A `[presets.<name>]` table as written; every field is optional.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetSection {
    theme: Option<String>,
    style: Option<String>,
    density: Option<Density>,
    layout: Option<DiffLayout>,
    token_emphasis: Option<bool>,
}

/// How a file's diff is laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffLayout {
    /// Removed lines above added lines, in one column.
    #[default]
    Unified,
    /// The base on the left and the head on the right, changed lines paired up.
    SideBySide,
}

impl DiffLayout {
    /// The other layout.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Unified => Self::SideBySide,
            Self::SideBySide => Self::Unified,
        }
    }

    /// The layout's name as the configuration file spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unified => "unified",
            Self::SideBySide => "side-by-side",
        }
    }
}

/// Whether approved hunks fold to their header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Approved {
    /// Approved hunks show their header alone, and fully reviewed files their header alone,
    /// until the reviewer unfolds one.
    #[default]
    Folded,
    /// Approved hunks and fully reviewed files show like any other.
    Shown,
}

impl Approved {
    /// The other choice.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Folded => Self::Shown,
            Self::Shown => Self::Folded,
        }
    }

    /// The choice's name as the configuration file spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Folded => "folded",
            Self::Shown => "shown",
        }
    }
}

/// What changed lines are drawn on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Background {
    /// Nothing: the editor background, with removed and added lines told apart by their
    /// sign, their text color or the change bar.
    None,
    /// The theme's line backgrounds mixed toward the editor background, as Zed draws them.
    Muted,
    /// The theme's saturated line backgrounds, as delta draws them.
    #[default]
    Bold,
}

impl Background {
    /// The background's name as the configuration file spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Muted => "muted",
            Self::Bold => "bold",
        }
    }
}

/// How much air the diff's rows have; only the density moves rows, never a diff style.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// A terminal's line height, with each hunk right after the code before it.
    Compact,
    /// A little more line height, and a blank row before each hunk.
    #[default]
    Normal,
    /// An editor's spacing, as Zed has it: tall lines, and a blank row before each hunk.
    Comfortable,
}

impl Density {
    /// Every density, from the tightest.
    pub const ALL: [Self; 3] = [Self::Compact, Self::Normal, Self::Comfortable];

    /// The density's name as the configuration file spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Comfortable => "comfortable",
        }
    }
}

/// How hunk headers are drawn; they take one row either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Headers {
    /// A band across the width, as a terminal draws it.
    #[default]
    Bands,
    /// Quiet chips on the editor background, as an editor draws them.
    Chips,
}

impl Headers {
    /// The header look's name as the configuration file spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bands => "bands",
            Self::Chips => "chips",
        }
    }
}

/// A user diff style as the configuration file sets it; fields it leaves out take the
/// defaults, which are the bold style's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyleConfig {
    /// Whether the diff shows syntax colors.
    pub syntax: Option<bool>,
    /// What changed lines are drawn on.
    pub background: Option<Background>,
    /// How hunk headers are drawn.
    pub headers: Option<Headers>,
    /// Whether changed lines have a colored bar in the gutter.
    pub change_bar: Option<bool>,
}

/// A user preset as the configuration file sets it; fields it leaves out take their defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresetConfig {
    /// The theme's name, checked against the themes once they are loaded.
    pub theme: Option<String>,
    /// The diff style's name, checked against the styles.
    pub style: Option<String>,
    /// How much air the rows have.
    pub density: Option<Density>,
    /// How the diff is laid out.
    pub layout: Option<DiffLayout>,
    /// Whether changed tokens within a line are emphasized.
    pub token_emphasis: Option<bool>,
}

/// Everything the configuration file sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The code font.
    pub font: FontConfig,
    /// Unchanged lines shown around each hunk.
    pub context: usize,
    /// The diff style's name, checked against the styles; `None` for the default.
    pub diff_style: Option<String>,
    /// How much air the diff's rows have.
    pub density: Density,
    /// How the diff is laid out, as the reviewer asked for it; a narrow window may still show
    /// it unified.
    pub diff_layout: DiffLayout,
    /// Whether approved hunks fold to their header.
    pub approved: Approved,
    /// The removed-line background, as `0xRRGGBB`, overriding the theme's.
    pub removed: Option<u32>,
    /// The added-line background, likewise.
    pub added: Option<u32>,
    /// The theme's name.
    pub theme: Option<String>,
    /// Whether changed tokens within a line are emphasized.
    pub token_emphasis: bool,
    /// The pane widths.
    pub layout: LayoutConfig,
    /// Key overrides: an action's name and the keys that replace its defaults.
    pub keys: BTreeMap<String, Vec<String>>,
    /// User diff styles by name, in name order; one the file gets wrong is left out and
    /// named in `problems`.
    pub styles: BTreeMap<String, StyleConfig>,
    /// User presets by name, in name order, or why one cannot be used.
    pub presets: BTreeMap<String, Result<PresetConfig, String>>,
    /// What the file gets wrong that only costs the entries it is in.
    pub problems: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            context: 3,
            diff_style: None,
            density: Density::default(),
            diff_layout: DiffLayout::Unified,
            approved: Approved::Folded,
            removed: None,
            added: None,
            theme: None,
            token_emphasis: false,
            layout: LayoutConfig::default(),
            keys: BTreeMap::new(),
            styles: BTreeMap::new(),
            presets: BTreeMap::new(),
            problems: Vec::new(),
        }
    }
}

/// A setting the app remembers in the configuration file: `key` in `[table]`.
#[derive(Debug, Clone)]
pub struct Remembered {
    /// The top-level table, such as `diff`.
    pub table: &'static str,
    /// The key within it, such as `style`.
    pub key: &'static str,
    /// The value to set.
    pub value: toml_edit::Value,
}

/// Why a setting could not be remembered; the file is left as it was.
#[derive(Debug, thiserror::Error)]
pub enum RememberError {
    /// The file exists but cannot be read.
    #[error("cannot read {path}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML, or a table to write into is a plain value; rewriting it
    /// would lose what the reviewer wrote.
    #[error("cannot edit {path}: {message}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
    /// The file or its directory cannot be written.
    #[error("cannot write {path}")]
    Write {
        /// The file.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
}

/// Remembers settings changed in the app in the configuration file at `path`, all in one write,
/// keeping the rest of the file as it is: comments, formatting, presets and other settings.
///
/// # Errors
///
/// Returns an error, and leaves the file untouched, when it cannot be read, is not valid TOML,
/// has a plain value where a table should be, or cannot be written.
pub fn remember(path: &Path, values: &[Remembered]) -> Result<(), RememberError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(RememberError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let text = with_values(&text, values).map_err(|message| RememberError::Invalid {
        path: path.to_path_buf(),
        message,
    })?;
    let write = |source| RememberError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(write)?;
    }
    std::fs::write(path, text).map_err(write)
}

/// `text` with each of `values` set: a key replaced in place keeps its comments, a new key goes
/// to the end of its table, and a missing table is appended.
///
/// Tables are found as TOML defines them, so a header with a comment, an inline table or dotted
/// keys are all the same table.
fn with_values(text: &str, values: &[Remembered]) -> Result<String, String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|err: toml_edit::TomlError| err.message().to_owned())?;
    for setting in values {
        let item = document
            .as_table_mut()
            .entry(setting.table)
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let table = item
            .as_table_like_mut()
            .ok_or_else(|| format!("{} is not a table", setting.table))?;
        match table.get_mut(setting.key) {
            Some(toml_edit::Item::Value(existing)) => {
                // The old value's decor holds the spacing and a trailing comment.
                let decor = existing.decor().clone();
                *existing = setting.value.clone();
                *existing.decor_mut() = decor;
            }
            Some(_) => {
                return Err(format!(
                    "[{}] {} is a table, not a value",
                    setting.table, setting.key
                ));
            }
            None => {
                table.insert(setting.key, toml_edit::Item::Value(setting.value.clone()));
            }
        }
    }
    Ok(document.to_string())
}

/// Parses a `#rrggbb` color.
fn hex_color(text: &str) -> Option<u32> {
    let digits = text.strip_prefix('#')?;
    (digits.len() == 6).then_some(())?;
    u32::from_str_radix(digits, 16).ok()
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FontSection {
    family: Option<String>,
    size: Option<f32>,
    weight: Option<Weight>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Weight {
    Number(f32),
    Name(String),
}

/// Returns the configuration file's path, or `None` without a configuration directory.
#[must_use]
pub fn path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match xdg_config_home.filter(|dir| dir.is_absolute()) {
        Some(dir) => dir.to_path_buf(),
        None => home?.join(".config"),
    };
    Some(base.join("code-review").join("config.toml"))
}

/// Reads the settings from `path`; a missing file gives the defaults.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    parse(&text).map_err(|message| ConfigError::Invalid {
        path: path.to_path_buf(),
        message,
    })
}

/// Parses the settings from the text of a configuration file.
///
/// # Errors
///
/// Returns a description of what is wrong.
pub fn parse(text: &str) -> Result<Config, String> {
    let file: File = toml::from_str(text).map_err(|err| err.message().to_owned())?;
    let defaults = Config::default();
    let mut problems = Vec::new();
    let mut styles = BTreeMap::new();
    for (name, style) in file.styles {
        match parse_style(style) {
            Ok(style) => {
                styles.insert(name, style);
            }
            Err(problem) => problems.push(format!("[styles.{name}] cannot be used: {problem}")),
        }
    }
    let color = |value: Option<String>| match value {
        None => Ok(None),
        Some(text) => hex_color(&text)
            .map(Some)
            .ok_or_else(|| format!("{text:?} is not a #rrggbb color")),
    };
    Ok(Config {
        font: parse_font(file.font)?,
        context: file.view.context.unwrap_or(defaults.context).min(1000),
        diff_style: file.diff.style,
        density: file.diff.density.unwrap_or(defaults.density),
        diff_layout: file.diff.layout.unwrap_or(defaults.diff_layout),
        approved: file.diff.approved.unwrap_or(defaults.approved),
        removed: color(file.diff.removed)?,
        added: color(file.diff.added)?,
        theme: file.theme.name,
        token_emphasis: file.diff.token_emphasis.unwrap_or(defaults.token_emphasis),
        layout: LayoutConfig {
            sidebar: file
                .layout
                .sidebar
                .filter(|width| width.is_finite())
                .unwrap_or(DEFAULT_SIDEBAR_WIDTH),
            source: file.layout.source.filter(|width| width.is_finite()),
            sidebar_visible: file.layout.sidebar_visible.unwrap_or(true),
        },
        keys: file
            .keys
            .into_iter()
            .map(|(action, keys)| {
                let keys = match keys {
                    KeyList::One(key) => vec![key],
                    KeyList::Many(keys) => keys,
                };
                (action, keys)
            })
            .collect(),
        styles,
        presets: file
            .presets
            .into_iter()
            .map(|(name, preset)| (name, parse_preset(preset)))
            .collect(),
        problems,
    })
}

/// A `[styles.<name>]` table, or what is wrong with it.
fn parse_style(value: toml::Value) -> Result<StyleConfig, String> {
    let StyleSection {
        syntax,
        background,
        headers,
        change_bar,
        density,
    } = value
        .try_into()
        .map_err(|err: toml::de::Error| err.message().to_owned())?;
    if density.is_some() {
        return Err(
            "it sets density, but a diff style is colors only; set density under \
                    [diff] or in a preset"
                .to_owned(),
        );
    }
    Ok(StyleConfig {
        syntax,
        background,
        headers,
        change_bar,
    })
}

/// A `[presets.<name>]` table, or what is wrong with it.
fn parse_preset(value: toml::Value) -> Result<PresetConfig, String> {
    let PresetSection {
        theme,
        style,
        density,
        layout,
        token_emphasis,
    } = value
        .try_into()
        .map_err(|err: toml::de::Error| err.message().to_owned())?;
    Ok(PresetConfig {
        theme,
        style,
        density,
        layout,
        token_emphasis,
    })
}

fn parse_font(section: FontSection) -> Result<FontConfig, String> {
    let FontSection {
        family,
        size,
        weight,
    } = section;
    if let Some(size) = size
        && !(4.0..=96.0).contains(&size)
    {
        return Err(format!("font size {size} is outside 4 to 96 points"));
    }
    let weight = match weight {
        None => None,
        Some(Weight::Number(number)) if (100.0..=900.0).contains(&number) => Some(number),
        Some(Weight::Number(number)) => {
            return Err(format!("font weight {number} is outside 100 to 900"));
        }
        Some(Weight::Name(name)) => {
            Some(weight_from_name(&name).ok_or_else(|| format!("unknown font weight {name:?}"))?)
        }
    };
    Ok(FontConfig {
        family: family.filter(|family| !family.trim().is_empty()),
        size_points: size,
        weight,
    })
}

/// Maps a weight name, as font styles spell it, to its numeric weight.
#[must_use]
pub fn weight_from_name(name: &str) -> Option<f32> {
    let normalized: String = name
        .chars()
        .filter(|ch| !ch.is_whitespace() && *ch != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    Some(match normalized.as_str() {
        "thin" => 100.0,
        "extralight" | "ultralight" => 200.0,
        "light" => 300.0,
        "regular" | "normal" => 400.0,
        "medium" => 500.0,
        "semibold" | "demibold" => 600.0,
        "bold" => 700.0,
        "extrabold" | "ultrabold" => 800.0,
        "black" | "heavy" => 900.0,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_settings_parse_with_names_numbers_and_defaults() {
        let config = parse("[font]\nfamily = \"SF Mono\"\nsize = 14\nweight = \"Semi Bold\"\n")
            .unwrap()
            .font;
        assert_eq!(config.family.as_deref(), Some("SF Mono"));
        assert_eq!(config.size_points, Some(14.0));
        assert_eq!(config.weight, Some(600.0));
        assert_eq!(
            parse("[font]\nweight = 300\n").unwrap().font.weight,
            Some(300.0)
        );
        assert_eq!(parse("").unwrap(), Config::default());

        // Mistakes are reported rather than silently ignored.
        assert!(parse("[font]\nsize = 200\n").is_err());
        assert!(parse("[font]\nweight = \"chunky\"\n").is_err());
        assert!(parse("[font]\nfamliy = \"x\"\n").is_err());
    }

    #[test]
    fn view_and_diff_settings_parse() {
        let config = parse(
            "[view]\ncontext = 5\n[diff]\nstyle = \"plain\"\nremoved = \"#112233\"\n[theme]\nname = \"nordfox\"\n",
        )
        .unwrap();
        assert_eq!(config.context, 5);
        assert_eq!(config.diff_style.as_deref(), Some("plain"));
        assert_eq!(config.removed, Some(0x0011_2233));
        assert_eq!(config.added, None);
        assert_eq!(config.theme.as_deref(), Some("nordfox"));
        assert!(parse("[diff]\nadded = \"green\"\n").is_err());
        assert!(parse("[diff]\nstyle = 3\n").is_err());
    }

    fn setting(table: &'static str, key: &'static str, value: &str) -> Remembered {
        Remembered {
            table,
            key,
            value: value.into(),
        }
    }

    /// A remembered value is replaced in place, added to an existing table, or appended in a
    /// new one.
    #[test]
    fn values_are_remembered_in_place() {
        let plain = setting("diff", "style", "plain");
        assert_eq!(
            with_values("[font]\nsize = 14\n", std::slice::from_ref(&plain)).unwrap(),
            "[font]\nsize = 14\n\n[diff]\nstyle = \"plain\"\n"
        );
        assert_eq!(
            with_values(
                "[diff]\nstyle = \"syntax\"\nadded = \"#000000\"\n[view]\n",
                std::slice::from_ref(&plain)
            )
            .unwrap(),
            "[diff]\nstyle = \"plain\"\nadded = \"#000000\"\n[view]\n"
        );
        let added = with_values(
            "[diff]\nadded = \"#000000\"\n",
            std::slice::from_ref(&plain),
        )
        .unwrap();
        assert_eq!(parse(&added).unwrap().diff_style.as_deref(), Some("plain"));
        let keys = parse("[keys]\nNextUnit = \"ctrl-j\"\nToggleOverview = []\n").unwrap();
        assert_eq!(keys.keys["NextUnit"], ["ctrl-j"]);
        assert_eq!(keys.keys["ToggleOverview"], Vec::<String>::new());
        let width = Remembered {
            table: "layout",
            key: "sidebar",
            value: 320.into(),
        };
        let widths = with_values(&added, &[width]).unwrap();
        assert_eq!(parse(&widths).unwrap().layout.sidebar, 320.0);
    }

    /// A header with a comment is the same table, so remembering a value never appends a
    /// second `[diff]`, which would make the file invalid; comments, user presets and other
    /// settings survive, and several values land in one edit.
    #[test]
    fn commented_headers_keep_one_table_and_every_comment() {
        let text = "\
# My review settings.
[diff] # colors
style = \"syntax\"  # delta's look
token_emphasis = false

[presets.mine]
theme = \"nordfox\" # a calm one

[keys]
NextHunk = [\"j\"]
";
        let updated = with_values(
            text,
            &[
                setting("diff", "style", "plain"),
                setting("diff", "layout", "side-by-side"),
                setting("theme", "name", "nordfox"),
            ],
        )
        .unwrap();
        assert_eq!(updated.matches("[diff]").count(), 1, "{updated}");
        for kept in [
            "# My review settings.",
            "[diff] # colors",
            "style = \"plain\"  # delta's look",
            "theme = \"nordfox\" # a calm one",
            "NextHunk = [\"j\"]",
        ] {
            assert!(updated.contains(kept), "lost {kept:?} in:\n{updated}");
        }
        let config = parse(&updated).unwrap();
        assert_eq!(config.diff_style.as_deref(), Some("plain"));
        assert_eq!(config.diff_layout, DiffLayout::SideBySide);
        assert_eq!(config.theme.as_deref(), Some("nordfox"));
        assert_eq!(
            config.presets["mine"].as_ref().unwrap().theme.as_deref(),
            Some("nordfox"),
            "the user's preset is untouched"
        );
        assert!(!config.token_emphasis);
    }

    /// Inline tables and dotted keys are tables too, and are edited where they are.
    #[test]
    fn inline_tables_and_dotted_keys_are_edited_in_place() {
        let inline = with_values(
            "diff = { style = \"syntax\" }\n",
            &[setting("diff", "style", "plain")],
        )
        .unwrap();
        assert_eq!(inline, "diff = { style = \"plain\" }\n");
        let dotted = with_values(
            "diff.style = \"syntax\"\n",
            &[setting("diff", "style", "plain")],
        )
        .unwrap();
        assert_eq!(dotted, "diff.style = \"plain\"\n");
        assert_eq!(parse(&dotted).unwrap().diff_style.as_deref(), Some("plain"));
    }

    /// A file that is not valid TOML, or has a value where a table belongs, is never rewritten:
    /// the reviewer's text would be lost.
    #[test]
    fn broken_files_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[diff\nstyle = 1\n").unwrap();
        let err = remember(&path, &[setting("diff", "style", "plain")]).unwrap_err();
        assert!(matches!(err, RememberError::Invalid { .. }), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[diff\nstyle = 1\n"
        );
        assert!(with_values("diff = 3\n", &[setting("diff", "style", "plain")]).is_err());

        // A missing file and its directory are created.
        let fresh = dir.path().join("new").join("config.toml");
        remember(&fresh, &[setting("theme", "name", "nordfox")]).unwrap();
        assert_eq!(
            parse(&std::fs::read_to_string(&fresh).unwrap())
                .unwrap()
                .theme
                .as_deref(),
            Some("nordfox")
        );
    }

    /// Layouts, styles and presets parse by the names the file uses.
    #[test]
    fn looks_styles_and_presets_parse() {
        let config = parse(
            "[diff]\nlayout = \"side-by-side\"\nstyle = \"calm\"\ndensity = \"compact\"\n\
             [styles.calm]\nsyntax = true\nbackground = \"muted\"\nheaders = \"chips\"\n\
             change_bar = true\n\
             [presets.calm]\ntheme = \"nordfox\"\nstyle = \"plain\"\ndensity = \"comfortable\"\n",
        )
        .unwrap();
        assert_eq!(config.diff_layout, DiffLayout::SideBySide);
        assert_eq!(config.diff_style.as_deref(), Some("calm"));
        assert_eq!(config.density, Density::Compact);
        assert_eq!(
            config.styles["calm"],
            StyleConfig {
                syntax: Some(true),
                background: Some(Background::Muted),
                headers: Some(Headers::Chips),
                change_bar: Some(true),
            }
        );
        assert_eq!(
            config.presets["calm"],
            Ok(PresetConfig {
                theme: Some("nordfox".to_owned()),
                style: Some("plain".to_owned()),
                density: Some(Density::Comfortable),
                ..PresetConfig::default()
            })
        );
        assert_eq!(
            parse("").unwrap().density,
            Density::Normal,
            "normal by default"
        );
        // A style is colors only; density set in one says where it belongs, and costs only
        // that style.
        let config = parse(
            "[diff]\nstyle = \"calm\"\n[styles.calm]\ndensity = \"comfortable\"\n\
             [styles.fine]\nsyntax = false\n",
        )
        .unwrap();
        assert_eq!(config.diff_style.as_deref(), Some("calm"));
        assert!(!config.styles.contains_key("calm"));
        assert!(config.styles.contains_key("fine"));
        assert_eq!(config.problems.len(), 1);
        assert!(
            config.problems[0].contains("[styles.calm] cannot be used: it sets density")
                && config.problems[0].contains("[diff]"),
            "{:?}",
            config.problems
        );
        // A mistake in a preset or a style costs only that entry, never the whole file.
        let config = parse(
            "[font]\nsize = 15\n[presets.calm]\ncolour = \"red\"\n[presets.fine]\n\
             theme = \"nordfox\"\n[styles.calm]\nbackground = \"loud\"\n",
        )
        .unwrap();
        assert_eq!(config.font.size_points, Some(15.0));
        let calm = config.presets["calm"].as_ref().unwrap_err();
        assert!(calm.contains("colour"), "{calm}");
        assert!(config.presets["fine"].is_ok());
        assert!(config.styles.is_empty());
        assert!(
            config.problems[0].starts_with("[styles.calm] cannot be used:"),
            "{:?}",
            config.problems
        );
        assert!(parse("[diff]\nlayout = \"split\"\n").is_err());
    }

    /// Approved hunks fold unless the file says to show them.
    #[test]
    fn approved_hunks_fold_by_default() {
        assert_eq!(parse("").unwrap().approved, Approved::Folded);
        let config = parse("[diff]\napproved = \"shown\"\n").unwrap();
        assert_eq!(config.approved, Approved::Shown);
        assert_eq!(config.approved.toggled(), Approved::Folded);
        assert!(parse("[diff]\napproved = \"open\"\n").is_err());
    }

    /// 13 points is 17.33 logical pixels; gpui applies the display scale on top.
    #[test]
    fn points_convert_to_logical_pixels_once() {
        assert!((points_to_pixels(13.0) - 17.333).abs() < 0.01);
        assert!((FontConfig::default().size_pixels() - 17.333).abs() < 0.01);
    }

    #[test]
    fn the_file_lives_under_the_config_directory() {
        assert_eq!(
            path(Some(Path::new("/c")), Some(Path::new("/h"))),
            Some(PathBuf::from("/c/code-review/config.toml"))
        );
        assert_eq!(
            path(None, Some(Path::new("/h"))),
            Some(PathBuf::from("/h/.config/code-review/config.toml"))
        );
        assert_eq!(path(Some(Path::new("relative")), None), None);
    }
}
