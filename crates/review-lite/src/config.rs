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
//! style = "syntax"  # or "plain", like git's colored diff
//! removed = "#5f1519"   # overrides the theme's line backgrounds
//! added = "#14471f"
//! token_emphasis = false
//!
//! [theme]
//! name = "carbonfox"  # or tokyonight-night, nightfox, one-dark, …; T cycles them
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
    style: Option<DiffStyle>,
    removed: Option<String>,
    added: Option<String>,
    token_emphasis: Option<bool>,
}

/// How diff lines are colored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffStyle {
    /// Syntax-highlighted code on tinted line backgrounds.
    #[default]
    Syntax,
    /// A classic colored diff: red and green text, no backgrounds.
    Plain,
}

impl DiffStyle {
    /// The other style.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Syntax => Self::Plain,
            Self::Plain => Self::Syntax,
        }
    }

    /// The style's name as the configuration file spells it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Plain => "plain",
        }
    }
}

/// Everything the configuration file sets.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// The code font.
    pub font: FontConfig,
    /// Whether hunk headers show unit IDs.
    /// Unchanged lines shown around each hunk.
    pub context: usize,
    /// How diff lines are colored.
    pub diff_style: DiffStyle,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            context: 3,
            diff_style: DiffStyle::Syntax,
            removed: None,
            added: None,
            theme: None,
            token_emphasis: false,
            layout: LayoutConfig::default(),
            keys: BTreeMap::new(),
        }
    }
}

/// Remembers settings changed in the app, each a `(table, key, TOML value)`, in the
/// configuration file at `path`, keeping the rest of the file as it is.
///
/// # Errors
///
/// Returns an error when the file cannot be read or written.
pub fn remember(path: &Path, values: &[(&str, &str, String)]) -> std::io::Result<()> {
    let mut text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    for (table, key, value) in values {
        text = with_value(&text, table, key, value);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

/// `text` with `key = value` set in `[table]`: replaced in place, added to an existing table,
/// or appended in a new one.
fn with_value(text: &str, table: &str, key: &str, value: &str) -> String {
    let line = format!("{key} = {value}");
    let header = format!("[{table}]");
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let section = lines.iter().position(|line| line.trim() == header);
    if let Some(start) = section {
        let end = lines
            .iter()
            .skip(start + 1)
            .position(|line| line.trim_start().starts_with('['))
            .map_or(lines.len(), |offset| start + 1 + offset);
        let existing = (start + 1..end).find(|&index| {
            lines.get(index).is_some_and(|line| {
                line.split_once('=')
                    .is_some_and(|(name, _)| name.trim() == key)
            })
        });
        if let Some(slot) = existing.and_then(|index| lines.get_mut(index)) {
            *slot = line;
        } else {
            lines.insert(start + 1, line);
        }
    } else {
        if lines.last().is_some_and(|last| !last.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(header);
        lines.push(line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
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
    let color = |value: Option<String>| match value {
        None => Ok(None),
        Some(text) => hex_color(&text)
            .map(Some)
            .ok_or_else(|| format!("{text:?} is not a #rrggbb color")),
    };
    Ok(Config {
        font: parse_font(file.font)?,
        context: file.view.context.unwrap_or(defaults.context).min(1000),
        diff_style: file.diff.style.unwrap_or(defaults.diff_style),
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
        assert_eq!(config.diff_style, DiffStyle::Plain);
        assert_eq!(config.removed, Some(0x0011_2233));
        assert_eq!(config.added, None);
        assert_eq!(config.theme.as_deref(), Some("nordfox"));
        assert!(parse("[diff]\nadded = \"green\"\n").is_err());
        assert!(parse("[diff]\nstyle = \"fancy\"\n").is_err());
    }

    /// A remembered value is replaced in place, added to an existing table, or appended.
    #[test]
    fn values_are_remembered_in_place() {
        let plain = "\"plain\"";
        assert_eq!(
            with_value("[font]\nsize = 14\n", "diff", "style", plain),
            "[font]\nsize = 14\n\n[diff]\nstyle = \"plain\"\n"
        );
        assert_eq!(
            with_value(
                "[diff]\nstyle = \"syntax\"\nadded = \"#000000\"\n[view]\n",
                "diff",
                "style",
                plain
            ),
            "[diff]\nstyle = \"plain\"\nadded = \"#000000\"\n[view]\n"
        );
        let added = with_value("[diff]\nadded = \"#000000\"\n", "diff", "style", plain);
        assert_eq!(parse(&added).unwrap().diff_style, DiffStyle::Plain);
        let keys = parse("[keys]\nNextUnit = \"ctrl-j\"\nToggleOverview = []\n").unwrap();
        assert_eq!(keys.keys["NextUnit"], ["ctrl-j"]);
        assert_eq!(keys.keys["ToggleOverview"], Vec::<String>::new());
        let widths = with_value(&added, "layout", "sidebar", "320");
        assert_eq!(parse(&widths).unwrap().layout.sidebar, 320.0);
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
