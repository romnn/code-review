//! The settings list's content: its title, search, sections, entries and footer.

use super::{Palette, Piece, Weight};
use crate::color::Hsla;
use crate::model::settings::{Choice, Entry, Line, entries, lines};
use crate::model::{Model, plural};
use crate::theme::Theme;

/// How wide the settings panel is over the diff, when the files pane has no room for it, in
/// cells of the panel's text.
pub const PANEL_CELLS: f32 = 46.0;

/// The most of the window's width the panel takes over the diff, so a preview stays in view.
pub const PANEL_MAX_SHARE: f32 = 0.6;

/// The narrowest files pane the panel takes the place of, in cells of the panel's text.
pub const PANEL_MIN_CELLS: f32 = 28.0;

/// The narrowest panel that lists each entry's detail beside its label, in cells of the
/// panel's text; a narrower one shows only the cursor's, in its footer.
pub const DETAILS_MIN_CELLS: f32 = 43.0;

/// The cells of an entry's label column while details show beside it, so swatches and details
/// line up down the list.
pub const LABEL_CELLS: f32 = 20.0;

/// The panel's title, semibold.
#[must_use]
pub fn title(palette: &Palette) -> Piece {
    Piece::new("Settings", palette.text).weighted(Weight::Semibold)
}

/// What the list says when the search matches nothing.
#[must_use]
pub fn empty(palette: &Palette) -> Piece {
    Piece::new(
        "No setting matches; Esc goes back to the list.",
        palette.faint,
    )
}

/// The search row: the `/` and how many entries show, accented while the field has the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    /// The `/` before the field.
    pub slash: Piece,
    /// How many settings there are, or how many of them the search shows.
    pub count: Piece,
    /// The rule under the row.
    pub rule: Hsla,
}

/// The open list's search row.
#[must_use]
pub fn search(model: &Model, palette: &Palette) -> Option<Search> {
    let panel = model.settings_panel.as_ref()?;
    let total = entries(model).len();
    let count = if panel.query.is_empty() {
        plural(total, "setting")
    } else {
        format!("{}/{total}", model.shown_entries().len())
    };
    let focused = panel.searching;
    Some(Search {
        slash: Piece::new(
            "/",
            if focused {
                palette.accent
            } else {
                palette.faint
            },
        ),
        count: Piece::new(count, palette.faint),
        rule: if focused {
            palette.accent
        } else {
            palette.hairline
        },
    })
}

/// A line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsLine {
    /// A section's title, in capitals, low in its row.
    Section {
        /// The title.
        title: Piece,
        /// The hairline above every section but the first, which sets the groups apart.
        rule: Option<Hsla>,
    },
    /// An entry.
    Entry(Box<SettingsEntry>),
}

/// An entry of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsEntry {
    /// What applying it sets, for a press.
    pub choice: Choice,
    /// `●` on what is applied, in its own column.
    pub marker: Piece,
    /// The label, bright when applied or under the cursor.
    pub label: Piece,
    /// A theme's colors, as small squares.
    pub swatch: Option<Swatch>,
    /// What it does, beside the label when the panel is wide enough.
    pub detail: Piece,
    /// The cursor's background.
    pub background: Option<Hsla>,
    /// The cursor's bar along the row's left edge.
    pub bar: Option<Hsla>,
}

/// Four squares of a theme's colors: its background, removed and added lines, and accent,
/// framed in its hairline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swatch {
    /// The frame's color.
    pub border: Hsla,
    /// The squares, left to right.
    pub squares: [Hsla; 4],
}

/// The swatch of `theme`.
#[must_use]
pub fn swatch(theme: &Theme) -> Swatch {
    Swatch {
        border: theme.ui.hairline,
        squares: [
            theme.ui.background,
            theme.diff.removed_line,
            theme.diff.added_line,
            theme.ui.accent,
        ],
    }
}

/// The open list's lines: each section's title before its entries.
#[must_use]
pub fn settings_lines(model: &Model, palette: &Palette) -> Vec<SettingsLine> {
    let Some(panel) = model.settings_panel.as_ref() else {
        return Vec::new();
    };
    let shown = model.shown_entries();
    lines(&shown)
        .into_iter()
        .enumerate()
        .filter_map(|(position, line)| match line {
            Line::Section(title) => Some(SettingsLine::Section {
                title: Piece::new(title.to_uppercase(), palette.faint),
                rule: (position > 0).then_some(palette.hairline),
            }),
            Line::Entry(index) => {
                let entry = shown.get(index)?;
                Some(SettingsLine::Entry(Box::new(settings_entry(
                    model,
                    palette,
                    entry,
                    entry.choice == panel.cursor,
                ))))
            }
        })
        .collect()
}

/// `entry`, under the cursor or not.
fn settings_entry(
    model: &Model,
    palette: &Palette,
    entry: &Entry,
    under_cursor: bool,
) -> SettingsEntry {
    let applied = model.is_applied(&entry.choice);
    let swatch = match &entry.choice {
        Choice::Theme(name) => model.find_theme(name).map(swatch),
        Choice::Style(_)
        | Choice::Density(_)
        | Choice::Layout(_)
        | Choice::Emphasis(_)
        | Choice::Approved(_)
        | Choice::AutoApprove(_)
        | Choice::FontSize(_) => None,
    };
    SettingsEntry {
        choice: entry.choice.clone(),
        marker: Piece::new(if applied { "●" } else { "" }, palette.accepted),
        label: Piece::new(
            entry.label.clone(),
            if applied || under_cursor {
                palette.text
            } else {
                palette.dim
            },
        ),
        swatch,
        detail: Piece::new(entry.detail.clone(), palette.faint),
        background: under_cursor.then_some(palette.raised),
        bar: under_cursor.then_some(palette.accent),
    }
}

/// The footer: whether the diff shows a preview, and what the entry under the cursor does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    /// `preview` in the accent, while the diff previews the cursor's entry.
    pub preview: Option<Piece>,
    /// What the entry under the cursor does.
    pub detail: Option<Piece>,
}

/// The open list's footer.
#[must_use]
pub fn footer(model: &Model, palette: &Palette) -> Footer {
    let detail = model.settings_panel.as_ref().and_then(|panel| {
        model
            .shown_entries()
            .into_iter()
            .find(|entry| entry.choice == panel.cursor)
            .map(|entry| Piece::new(entry.detail, palette.faint))
    });
    Footer {
        preview: model
            .previewing()
            .then(|| Piece::new("preview", palette.accent)),
        detail,
    }
}
