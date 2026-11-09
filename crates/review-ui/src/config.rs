//! The user's settings for the review front ends, read from
//! `$XDG_CONFIG_HOME/code-review/config.toml`.
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
//! change_bar = true     # a colored bar in the gutter of changed lines
//!
//! [theme]
//! name = "carbonfox"  # or tokyonight-night, nightfox, one-dark, …; T opens the settings
//!
//! [review]
//! auto_approve_formatting = true  # proven formatting-only hunks count as approved, unmarked
//!
//! [lsp]
//! enabled = true    # false never starts a language server; `g d` then says it is off
//!
//! [keys]            # replace an action's keys; `?` in the app lists the actions
//! NextUnit = ["n", "ctrl-j"]
//! ToggleOverview = []   # unbound
//!
//! [layout]          # remembered when the panes are resized or the sidebar toggled
//! sidebar = 300     # review-gui's files pane, in logical pixels
//! tui_sidebar = 32  # review-tui's files pane, in terminal cells
//! source = 600      # the definition pane; absent for 42% of the window
//! sidebar_visible = true
//! ```
//!
//! Choices made in the app are written back with [`remember`], which edits the file as TOML and
//! keeps everything else in it: comments, styles, keys and settings it does not change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Points per inch, and the pixels per inch that a logical pixel assumes: gpui's logical pixels
/// are 1/96 inch before the display scale factor, which gpui applies itself.
const POINTS_PER_INCH: f32 = 72.0;
const PIXELS_PER_INCH: f32 = 96.0;

/// The defaults for a configuration file that cannot be read whole, keeping its
/// `[review] auto_approve_formatting` as `cr` reads it: that section alone, so a mistake
/// elsewhere leaves it standing, and automatic approval off when even that cannot be read.
///
/// A broken file thus never approves units the reviewer may have asked to review, and the apps
/// and `cr`, which reads only that section, agree on which units need the reviewer.
#[must_use]
pub fn fallback(path: &Path) -> Config {
    let auto_approve_formatting = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|file| match file.get("review") {
            None => Some(true),
            Some(review) => match review.as_table()?.get("auto_approve_formatting") {
                None => Some(true),
                Some(value) => value.as_bool(),
            },
        })
        .unwrap_or(false);
    Config {
        auto_approve_formatting,
        ..Config::default()
    }
}

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
    #[serde(default)]
    review: ReviewSection,
    #[serde(default)]
    lsp: LspSection,
    /// Read table by table, so a mistake in one style leaves the others usable.
    #[serde(default)]
    styles: BTreeMap<String, toml::Value>,
    /// Read only to say that presets are gone, rather than reject an older file whole.
    presets: Option<serde::de::IgnoredAny>,
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
    tui_sidebar: Option<f32>,
    source: Option<f32>,
    sidebar_visible: Option<bool>,
}

/// The sidebar's default width in logical pixels.
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 300.0;

/// The pane widths and whether the sidebar shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutConfig {
    /// The sidebar's width in logical pixels, as review-gui remembers it.
    pub sidebar: f32,
    /// The sidebar's width in terminal cells, as review-tui remembers it; `None` for its
    /// default.
    pub tui_sidebar: Option<f32>,
    /// The definition pane's width in logical pixels; `None` gives it 42% of the window.
    pub source: Option<f32>,
    /// Whether the sidebar shows.
    pub sidebar_visible: bool,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            sidebar: DEFAULT_SIDEBAR_WIDTH,
            tui_sidebar: None,
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
struct ReviewSection {
    auto_approve_formatting: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LspSection {
    enabled: Option<bool>,
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
    /// Read only to say it is gone: hunk headers have one look.
    headers: Option<serde::de::IgnoredAny>,
    change_bar: Option<bool>,
    /// Read only to say where it belongs now: a style never changes the spacing.
    density: Option<serde::de::IgnoredAny>,
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
    /// A terminal's line height, with each hunk and note right after the code before it.
    Compact,
    /// A little more line height, a blank row before each hunk, and one above and below each
    /// note.
    #[default]
    Normal,
    /// An editor's spacing, as Zed has it: tall lines and the normal density's blank rows; in
    /// a terminal, whose lines cannot grow, also a blank row between each gap of unchanged
    /// lines and the hunk after it.
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

/// A user diff style as the configuration file sets it; fields it leaves out take the
/// defaults, which are the bold style's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyleConfig {
    /// Whether the diff shows syntax colors.
    pub syntax: Option<bool>,
    /// What changed lines are drawn on.
    pub background: Option<Background>,
    /// Whether changed lines have a colored bar in the gutter.
    pub change_bar: Option<bool>,
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
    /// Whether proven formatting-only units count as approved without a mark; see
    /// [`review_core::Standing`].
    pub auto_approve_formatting: bool,
    /// Whether language servers may start, for go to definition; off keeps them from ever
    /// running, at the cost of that lookup.
    pub language_servers: bool,
    /// The pane widths.
    pub layout: LayoutConfig,
    /// Key overrides: an action's name and the keys that replace its defaults.
    pub keys: BTreeMap<String, Vec<String>>,
    /// User diff styles by name, in name order; one the file gets wrong is left out and
    /// named in `problems`.
    pub styles: BTreeMap<String, StyleConfig>,
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
            auto_approve_formatting: true,
            language_servers: true,
            layout: LayoutConfig::default(),
            keys: BTreeMap::new(),
            styles: BTreeMap::new(),
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
/// keeping the rest of the file as it is: comments, formatting, styles and other settings.
///
/// The new text replaces the file in one step, so another program reading it, such as the other
/// front end, sees the old file or the new one and never half of it.
/// A symlinked file is written where the link points, following a chain of links even to a file
/// that does not exist yet, and the links stay.
/// A file with other hard links is rewritten in place instead, so every name keeps the new text;
/// a reader may then see it half written.
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
    // Dotfile managers link the configuration in; replacing a link, or one name of a file with
    // several, would cut the configuration off from where it is kept.
    let target = followed(path).map_err(write)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(write)?;
    }
    if has_other_names(&target).map_err(write)? {
        rewrite_file(&target, text.as_bytes()).map_err(write)
    } else {
        replace_file(&target, text.as_bytes()).map_err(write)
    }
}

/// How many symbolic links [`followed`] follows before it gives up, as Linux does.
const MAX_LINKS: usize = 40;

/// `path` with every symbolic link at its end followed, even to a file that does not exist
/// yet; a relative link is relative to the directory the link is in.
fn followed(path: &Path) -> std::io::Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = std::fs::read_link(&path)?;
                path = match path.parent() {
                    Some(directory) => directory.join(target),
                    None => target,
                };
            }
            Ok(_) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::other(format!(
        "more than {MAX_LINKS} symbolic links"
    )))
}

/// Whether the file at `path` has other hard links, which a rename would leave with the old
/// text.
fn has_other_names(path: &Path) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(hard_links(&metadata) > 1),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// How many hard links a file with `metadata` has.
#[cfg(unix)]
fn hard_links(metadata: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::nlink(metadata)
}

/// How many hard links a file has: one as far as Rust can count them on this platform, so a
/// hard-linked configuration is replaced like any other there.
#[cfg(not(unix))]
fn hard_links(_: &std::fs::Metadata) -> u64 {
    1
}

/// Writes `contents` over the file at `path` in place, keeping it the same file.
fn rewrite_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Replaces the file at `path` with `contents` atomically: they go to a new file in the same
/// directory, which a rename then puts in `path`'s place, keeping the old file's permissions.
///
/// A rename within one directory never crosses file systems, so it replaces the file whole.
fn replace_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Tells apart the temporary files of one process's concurrent writes.
    static WRITES: AtomicU32 = AtomicU32::new(0);

    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a file path"))?;
    let directory = path.parent().unwrap_or_else(|| Path::new(""));
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err),
    };

    // A fresh hidden file beside the target; one left by a crashed write is never reused.
    let (temporary, mut file) = loop {
        let write = WRITES.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".{}.{write}.tmp", std::process::id()));
        let temporary = directory.join(temporary_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    };

    let written = (|| {
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        // The write failed already, and that is the error to report; a temporary file that
        // cannot be removed either is only a hidden leftover.
        std::fs::remove_file(&temporary).ok();
    }
    written
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
///
/// `cr` reads its `[review]` section from the same file.
#[must_use]
pub fn path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    review_core::store::default_config_file(xdg_config_home, home)
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
    if file.presets.is_some() {
        problems.push("presets are no longer supported; use the individual settings".to_owned());
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
        auto_approve_formatting: file
            .review
            .auto_approve_formatting
            .unwrap_or(defaults.auto_approve_formatting),
        language_servers: file.lsp.enabled.unwrap_or(defaults.language_servers),
        layout: LayoutConfig {
            sidebar: file
                .layout
                .sidebar
                .filter(|width| width.is_finite())
                .unwrap_or(DEFAULT_SIDEBAR_WIDTH),
            tui_sidebar: file.layout.tui_sidebar.filter(|width| width.is_finite()),
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
    if headers.is_some() {
        return Err("it sets headers, but hunk headers have one look now; remove it".to_owned());
    }
    if density.is_some() {
        return Err(
            "it sets density, but a diff style is colors only; set density under [diff]".to_owned(),
        );
    }
    Ok(StyleConfig {
        syntax,
        background,
        change_bar,
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
    use indoc::{formatdoc, indoc};

    use super::*;

    #[test]
    fn font_settings_parse_with_names_numbers_and_defaults() {
        let config = parse(indoc! {"
            [font]
            family = \"SF Mono\"
            size = 14
            weight = \"Semi Bold\"
        "})
        .unwrap()
        .font;
        assert_eq!(config.family.as_deref(), Some("SF Mono"));
        assert_eq!(config.size_points, Some(14.0));
        assert_eq!(config.weight, Some(600.0));
        assert_eq!(
            parse(indoc! {"
                [font]
                weight = 300
            "})
            .unwrap()
            .font
            .weight,
            Some(300.0)
        );
        assert_eq!(parse("").unwrap(), Config::default());

        // Mistakes are reported rather than silently ignored.
        assert!(
            parse(indoc! {"
                [font]
                size = 200
            "})
            .is_err()
        );
        assert!(
            parse(indoc! {"
                [font]
                weight = \"chunky\"
            "})
            .is_err()
        );
        // The misspelled key is deliberate: an unknown key must make the file fail to parse.
        // spellcheck:ignore-start
        assert!(
            parse(indoc! {"
                [font]
                famliy = \"x\"
            "})
            .is_err()
        );
        // spellcheck:ignore-end
    }

    #[test]
    fn view_and_diff_settings_parse() {
        let config = parse(indoc! {"
            [view]
            context = 5
            [diff]
            style = \"plain\"
            removed = \"#112233\"
            [theme]
            name = \"nordfox\"
        "})
        .unwrap();
        assert_eq!(config.context, 5);
        assert_eq!(config.diff_style.as_deref(), Some("plain"));
        assert_eq!(config.removed, Some(0x0011_2233));
        assert_eq!(config.added, None);
        assert_eq!(config.theme.as_deref(), Some("nordfox"));
        assert!(
            parse(indoc! {"
                [diff]
                added = \"green\"
            "})
            .is_err()
        );
        assert!(
            parse(indoc! {"
                [diff]
                style = 3
            "})
            .is_err()
        );
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
            with_values(
                indoc! {"
                    [font]
                    size = 14
                "},
                std::slice::from_ref(&plain)
            )
            .unwrap(),
            indoc! {"
                [font]
                size = 14

                [diff]
                style = \"plain\"
            "}
        );
        assert_eq!(
            with_values(
                indoc! {"
                    [diff]
                    style = \"syntax\"
                    added = \"#000000\"
                    [view]
                "},
                std::slice::from_ref(&plain)
            )
            .unwrap(),
            indoc! {"
                [diff]
                style = \"plain\"
                added = \"#000000\"
                [view]
            "}
        );
        let added = with_values(
            indoc! {"
                [diff]
                added = \"#000000\"
            "},
            std::slice::from_ref(&plain),
        )
        .unwrap();
        assert_eq!(parse(&added).unwrap().diff_style.as_deref(), Some("plain"));
        let keys = parse(indoc! {"
            [keys]
            NextUnit = \"ctrl-j\"
            ToggleOverview = []
        "})
        .unwrap();
        assert_eq!(keys.keys["NextUnit"], ["ctrl-j"]);
        assert_eq!(keys.keys["ToggleOverview"], Vec::<String>::new());
        let width = Remembered {
            table: "layout",
            key: "sidebar",
            value: 320.into(),
        };
        let widths = with_values(&added, &[width]).unwrap();
        assert_eq!(parse(&widths).unwrap().layout.sidebar, 320.0);
        // The terminal's width is a key of its own, which leaves the window's alone.
        let cells = Remembered {
            table: "layout",
            key: "tui_sidebar",
            value: 40.into(),
        };
        let both = parse(&with_values(&widths, &[cells]).unwrap()).unwrap();
        assert_eq!(both.layout.sidebar, 320.0);
        assert_eq!(both.layout.tui_sidebar, Some(40.0));
        assert_eq!(parse(&widths).unwrap().layout.tui_sidebar, None);
    }

    /// A header with a comment is the same table, so remembering a value never appends a
    /// second `[diff]`, which would make the file invalid; comments, user styles and other
    /// settings survive, and several values land in one edit.
    #[test]
    fn commented_headers_keep_one_table_and_every_comment() {
        let text = "\
# My review settings.
[diff] # colors
style = \"syntax\"  # delta's look
token_emphasis = false

[styles.mine]
background = \"muted\" # a calm one

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
            "background = \"muted\" # a calm one",
            "NextHunk = [\"j\"]",
        ] {
            assert!(
                updated.contains(kept),
                "{}",
                formatdoc! {"
                    lost {kept:?} in:
                    {updated}"}
            );
        }
        let config = parse(&updated).unwrap();
        assert_eq!(config.diff_style.as_deref(), Some("plain"));
        assert_eq!(config.diff_layout, DiffLayout::SideBySide);
        assert_eq!(config.theme.as_deref(), Some("nordfox"));
        assert_eq!(
            config.styles["mine"].background,
            Some(Background::Muted),
            "the user's style is untouched"
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
        std::fs::write(
            &path,
            indoc! {"
                [diff
                style = 1
            "},
        )
        .unwrap();
        let err = remember(&path, &[setting("diff", "style", "plain")]).unwrap_err();
        assert!(matches!(err, RememberError::Invalid { .. }), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            indoc! {"
                [diff
                style = 1
            "}
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

    /// Remembering replaces the file in one step through a temporary file beside it, which
    /// never stays behind; a symlinked file is written through, keeping the link.
    #[test]
    fn remembering_replaces_the_file_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            indoc! {"
                # mine
                [theme]
                name = \"carbonfox\"
            "},
        )
        .unwrap();
        // A reader that opened the old file before the write; Windows may refuse to replace a
        // file that is open, so only Unix checks this.
        #[cfg(unix)]
        let mut reader = std::fs::File::open(&path).unwrap();

        remember(&path, &[setting("theme", "name", "nordfox")]).unwrap();

        // The path has the new text, comments kept, and no temporary file is left.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            indoc! {"
                # mine
                [theme]
                name = \"nordfox\"
            "}
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["config.toml"]);

        // The open reader still reads the old file whole: the write replaced the file rather
        // than rewriting it in place.
        #[cfg(unix)]
        {
            let mut old = String::new();
            std::io::Read::read_to_string(&mut reader, &mut old).unwrap();
            assert_eq!(
                old,
                indoc! {"
                    # mine
                    [theme]
                    name = \"carbonfox\"
                "}
            );
        }

        // A symlinked configuration is written where the link points, and stays a link.
        #[cfg(unix)]
        {
            let real = dir.path().join("dotfiles").join("config.toml");
            std::fs::create_dir_all(real.parent().unwrap()).unwrap();
            std::fs::write(
                &real,
                indoc! {"
                    [theme]
                    name = \"carbonfox\"
                "},
            )
            .unwrap();
            let link = dir.path().join("linked.toml");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            remember(&link, &[setting("theme", "name", "nordfox")]).unwrap();
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                std::fs::read_to_string(&real).unwrap(),
                indoc! {"
                    [theme]
                    name = \"nordfox\"
                "}
            );
        }
    }

    /// A configuration linked to a file that does not exist yet, through a chain of relative
    /// links, is created where the chain ends, and every link stays a link.
    #[cfg(unix)]
    #[test]
    fn remembering_follows_links_to_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink("middle.toml", &link).unwrap();
        std::os::unix::fs::symlink("dotfiles/config.toml", dir.path().join("middle.toml")).unwrap();

        remember(&link, &[setting("theme", "name", "nordfox")]).unwrap();

        let is_link = |name: &str| {
            std::fs::symlink_metadata(dir.path().join(name))
                .unwrap()
                .file_type()
                .is_symlink()
        };
        assert!(is_link("config.toml") && is_link("middle.toml"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dotfiles/config.toml")).unwrap(),
            indoc! {"
                [theme]
                name = \"nordfox\"
            "}
        );
    }

    /// A configuration with another hard link is rewritten in place, so both names keep
    /// reading the same, new text.
    #[cfg(unix)]
    #[test]
    fn remembering_keeps_hard_links() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let other = dir.path().join("kept.toml");
        std::fs::write(
            &path,
            indoc! {"
                [theme]
                name = \"carbonfox\"
            "},
        )
        .unwrap();
        std::fs::hard_link(&path, &other).unwrap();

        remember(&path, &[setting("theme", "name", "nordfox")]).unwrap();

        for name in [&path, &other] {
            assert_eq!(
                std::fs::read_to_string(name).unwrap(),
                indoc! {"
                    [theme]
                    name = \"nordfox\"
                "}
            );
        }
    }

    /// Layouts and styles parse by the names the file uses.
    #[test]
    fn looks_and_styles_parse() {
        let config = parse(indoc! {"
            [diff]
            layout = \"side-by-side\"
            style = \"calm\"
            density = \"compact\"
            [styles.calm]
            syntax = true
            background = \"muted\"
            change_bar = true
        "})
        .unwrap();
        assert_eq!(config.diff_layout, DiffLayout::SideBySide);
        assert_eq!(config.diff_style.as_deref(), Some("calm"));
        assert_eq!(config.density, Density::Compact);
        assert_eq!(
            config.styles["calm"],
            StyleConfig {
                syntax: Some(true),
                background: Some(Background::Muted),
                change_bar: Some(true),
            }
        );
        assert_eq!(
            parse("").unwrap().density,
            Density::Normal,
            "normal by default"
        );
        // A style is colors only; density set in one says where it belongs, and costs only
        // that style.
        let config = parse(indoc! {"
            [diff]
            style = \"calm\"
            [styles.calm]
            density = \"comfortable\"
            [styles.fine]
            syntax = false
        "})
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
        // Hunk headers have one look; a style that still sets headers says so.
        let config = parse(indoc! {"
            [styles.calm]
            headers = \"chips\"
        "})
        .unwrap();
        assert!(!config.styles.contains_key("calm"));
        assert!(
            config.problems[0].contains("[styles.calm] cannot be used: it sets headers"),
            "{:?}",
            config.problems
        );
        // A mistake in a style costs only that style, never the whole file.
        let config = parse(indoc! {"
            [font]
            size = 15
            [styles.calm]
            background = \"loud\"
            [styles.fine]
            syntax = false
        "})
        .unwrap();
        assert_eq!(config.font.size_points, Some(15.0));
        assert!(!config.styles.contains_key("calm"));
        assert!(config.styles.contains_key("fine"));
        assert!(
            config.problems[0].starts_with("[styles.calm] cannot be used:"),
            "{:?}",
            config.problems
        );
        assert!(
            parse(indoc! {"
                [diff]
                layout = \"split\"
            "})
            .is_err()
        );
    }

    /// Presets are gone: an older file's `[presets]` costs nothing else and is named once,
    /// however many presets it has.
    #[test]
    fn presets_are_noted_once_and_ignored() {
        let config = parse(indoc! {"
            [diff]
            density = \"compact\"
            [presets.mine]
            theme = \"nordfox\"
            [presets.other]
            colour = \"red\"
        "})
        .unwrap();
        assert_eq!(config.density, Density::Compact);
        assert_eq!(
            config.problems,
            ["presets are no longer supported; use the individual settings"]
        );
    }

    /// Approved hunks fold unless the file says to show them.
    #[test]
    fn approved_hunks_fold_by_default() {
        assert_eq!(parse("").unwrap().approved, Approved::Folded);
        let config = parse(indoc! {"
            [diff]
            approved = \"shown\"
        "})
        .unwrap();
        assert_eq!(config.approved, Approved::Shown);
        assert_eq!(config.approved.toggled(), Approved::Folded);
        assert!(
            parse(indoc! {"
                [diff]
                approved = \"open\"
            "})
            .is_err()
        );
    }

    /// 13 points is 17.33 logical pixels; gpui applies the display scale on top.
    #[test]
    fn points_convert_to_logical_pixels_once() {
        assert!((points_to_pixels(13.0) - 17.333).abs() < 0.01);
        assert!((FontConfig::default().size_pixels() - 17.333).abs() < 0.01);
    }

    #[test]
    fn the_file_lives_under_the_config_directory() {
        // The temporary directory is absolute on every platform, which `/c` is not on Windows.
        let root = std::env::temp_dir();
        let (config, home) = (root.join("c"), root.join("h"));
        assert_eq!(
            path(Some(&config), Some(&home)),
            Some(config.join("code-review").join("config.toml"))
        );
        assert_eq!(
            path(None, Some(&home)),
            Some(home.join(".config").join("code-review").join("config.toml"))
        );
        assert_eq!(path(Some(Path::new("relative")), None), None);
    }

    /// Proven formatting-only hunks are approved automatically unless `[review]` turns it off;
    /// a misspelled key is an error rather than silently keeping the default.
    #[test]
    fn formatting_is_approved_automatically_by_default() {
        assert!(parse("").unwrap().auto_approve_formatting);
        assert!(
            !parse(indoc! {"
                [review]
                auto_approve_formatting = false
            "})
            .unwrap()
            .auto_approve_formatting
        );
        assert!(
            parse(indoc! {"
                [review]
                auto_approve = false
            "})
            .is_err()
        );
    }

    /// A file the apps cannot read whole keeps `[review] auto_approve_formatting` as `cr` reads
    /// it: a mistake in another section leaves it standing, and a file that is not TOML turns
    /// automatic approval off.
    #[test]
    fn a_broken_file_keeps_the_review_setting_as_cr_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fallback = |text: &str| {
            std::fs::write(&path, text).unwrap();
            assert!(load(&path).is_err(), "{text:?} is broken for the apps");
            fallback(&path).auto_approve_formatting
        };
        // The misspelled keys are deliberate: an unknown key makes the apps reject the file.
        // spellcheck:ignore-start
        assert!(!fallback(indoc! {"
            [review]
            auto_approve_formatting = false
            [diff]
            contex = 5
        "}));
        assert!(fallback(indoc! {"
            [diff]
            contex = 5
        "}));
        assert!(!fallback(indoc! {"
            [review
            auto_approve_formatting = true
        "}));
        assert!(!fallback(indoc! {"
            [review]
            auto_approve_formatting = 1
            [diff]
            contex = 5
        "}));
        // spellcheck:ignore-end
    }

    /// Language servers may start unless `[lsp] enabled = false`; a misspelled key is an error.
    #[test]
    fn language_servers_are_on_unless_turned_off() {
        assert!(parse("").unwrap().language_servers);
        assert!(
            !parse(indoc! {"
                [lsp]
                enabled = false
            "})
            .unwrap()
            .language_servers
        );
        assert!(
            parse(indoc! {"
                [lsp]
                enable = false
            "})
            .is_err()
        );
    }
}
