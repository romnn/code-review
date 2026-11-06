//! The settings list, opened with `T` or `,`: themes, diff styles, density, layout, token
//! emphasis, approved hunks and font size, keyboard first, with a search over every entry.
//!
//! The list and its search field own the keyboard apart, as two key contexts: the list takes
//! `j`/`k`, Space and `q`, the field takes everything typed, and Tab switches between them.
//! While the cursor rests on any entry, the review shows it as a preview over what the reviewer
//! applied, through the same transition that keeps the reviewer's place; moving off the entry,
//! filtering it out or closing the list in any way drops the preview.
//! The panel keeps the text size and row height it opened with, so previewing a density or a
//! font size changes the diff beside it and never the panel itself.
//! Applying moves the applied look and writes the entry's setting to the configuration; when
//! that write fails, the look stays applied and the status line says it could not be saved.

use super::{Model, Overlay, present::Places};
use crate::config::{Approved, Density, DiffLayout, Remembered, points_to_pixels};
use crate::frame::{ListScroll, Request};
use crate::keymap::Area;
use crate::look::{self, DiffStyle, FONT_SIZES, Look};
use crate::theme::Theme;

/// What an entry of the list sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// A theme, by name.
    Theme(String),
    /// A diff style, by name.
    Style(String),
    /// A row density.
    Density(Density),
    /// Unified or side-by-side diffs.
    Layout(DiffLayout),
    /// Whether changed tokens are underlined.
    Emphasis(bool),
    /// Whether approved hunks fold to their header.
    Approved(Approved),
    /// A code size in points.
    FontSize(u8),
}

/// What the diff previews over what is applied; `None` drops that part of a preview.
#[derive(Debug, Clone, Default)]
struct Preview {
    /// A look over the applied one.
    look: Option<Look>,
    /// A code size over the one in use, in logical pixels.
    code_size: Option<f32>,
    /// Approved hunks folded or shown, over what is in use.
    approved: Option<Approved>,
}

/// One entry of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The section it is listed under.
    pub section: &'static str,
    /// What it is called.
    pub label: String,
    /// What it does, in a few words.
    pub detail: String,
    /// More words the search finds it by, such as a theme's file name.
    pub(crate) keywords: String,
    /// What applying it sets.
    pub choice: Choice,
}

impl Entry {
    /// Whether every word of `query` appears in the entry's section, label or keywords; the
    /// detail only describes, so searching "head" finds no layout that merely mentions it.
    fn matches(&self, query: &str) -> bool {
        let haystack = format!("{} {} {}", self.section, self.label, self.keywords).to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| haystack.contains(word))
    }
}

/// A line of the list: a section's title, or an entry by its index among the shown entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// A section's title.
    Section(&'static str),
    /// An entry, by its index among the shown entries.
    Entry(usize),
}

/// The open settings list.
#[derive(Debug, Clone)]
pub struct Panel {
    /// The search field's identity; see [`super::FieldState::id`].
    pub(super) id: u64,
    /// What the search field holds.
    pub query: String,
    /// The entry under the cursor, kept by what it sets so filtering keeps it when it can.
    pub cursor: Choice,
    /// Whether the search field has the keys; otherwise the list has them.
    pub searching: bool,
    /// Where the list is scrolled.
    pub(crate) scroll: ListScroll,
    /// The code size in use apart from a font size's preview, in logical pixels.
    code_size: f32,
    /// Whether approved hunks fold apart from a preview.
    approved: Approved,
    /// Where the lists were when the panel opened; every preview starts from there, so one
    /// that moved a list to fit it never moves the place for good.
    places: Places,
    /// The code size the panel's own text size and row height follow, taken when it opened.
    pub text_code_size: f32,
}

/// Every entry of the list, in order: themes, then each setting's options.
#[must_use]
pub fn entries(app: &Model) -> Vec<Entry> {
    let mut entries = Vec::new();
    for theme in &app.settings.themes {
        entries.push(Entry {
            section: "Theme",
            label: theme.title.clone(),
            detail: String::new(),
            keywords: theme.name.clone(),
            choice: Choice::Theme(theme.name.clone()),
        });
    }
    for style in &app.settings.styles {
        entries.push(Entry {
            section: "Diff style",
            label: style.name.clone(),
            detail: style.summary(),
            keywords: "|".to_owned(),
            choice: Choice::Style(style.name.clone()),
        });
    }
    let mut option = |section: &'static str, label: &str, detail: &str, choice: Choice| {
        entries.push(Entry {
            section,
            label: label.to_owned(),
            detail: detail.to_owned(),
            keywords: String::new(),
            choice,
        });
    };
    for density in Density::ALL {
        let detail = match density {
            Density::Compact => "terminal rows",
            Density::Normal => "a little more air",
            Density::Comfortable => "Zed's spacing",
        };
        option("Density", density.name(), detail, Choice::Density(density));
    }
    option(
        "Layout",
        "unified",
        "removed lines above added lines · s",
        Choice::Layout(DiffLayout::Unified),
    );
    option(
        "Layout",
        "side by side",
        "base on the left, head on the right · s",
        Choice::Layout(DiffLayout::SideBySide),
    );
    option(
        "Token emphasis",
        "off",
        "changed lines only",
        Choice::Emphasis(false),
    );
    option(
        "Token emphasis",
        "on",
        "underline the tokens that changed",
        Choice::Emphasis(true),
    );
    option(
        "Approved hunks",
        "folded",
        "their header alone · zA",
        Choice::Approved(Approved::Folded),
    );
    option(
        "Approved hunks",
        "shown",
        "with their code · zA",
        Choice::Approved(Approved::Shown),
    );
    // A front end without a code size, such as a terminal, offers no font sizes.
    let sizes = if app.settings.front_end.capabilities().code_size {
        FONT_SIZES
    } else {
        &[]
    };
    for &size in sizes {
        let default = (look::points_of(app.default_code_size) - f32::from(size)).abs() < 0.5;
        option(
            "Font size",
            &format!("{size} pt"),
            if default { "default" } else { "" },
            Choice::FontSize(size),
        );
    }
    entries
}

/// The lines of `entries`: each section's title before its first entry.
#[must_use]
pub fn lines(entries: &[Entry]) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut section = None;
    for (index, entry) in entries.iter().enumerate() {
        if section != Some(entry.section) {
            section = Some(entry.section);
            lines.push(Line::Section(entry.section));
        }
        lines.push(Line::Entry(index));
    }
    lines
}

impl Model {
    /// The entries the search shows.
    #[must_use]
    pub fn shown_entries(&self) -> Vec<Entry> {
        let query = self
            .settings_panel
            .as_ref()
            .map(|panel| panel.query.clone())
            .unwrap_or_default();
        entries(self)
            .into_iter()
            .filter(|entry| entry.matches(&query))
            .collect()
    }

    /// The lines the open list shows, its sections' titles and entries; none while it is
    /// closed.
    #[must_use]
    pub(crate) fn settings_line_count(&self) -> usize {
        if self.settings_panel.is_none() {
            return 0;
        }
        lines(&self.shown_entries()).len()
    }

    /// Whether the list's search field has the keys.
    #[must_use]
    pub(crate) fn settings_searching(&self) -> bool {
        self.settings_panel
            .as_ref()
            .is_some_and(|panel| panel.searching)
    }

    /// The area the open settings list gives the keys to.
    pub(super) fn settings_area(&self) -> Area {
        if self.settings_searching() {
            Area::SettingsSearch
        } else {
            Area::SettingsList
        }
    }

    /// `T` and `,`: opens the list with its cursor on the theme in use.
    pub(super) fn open_settings(&mut self) {
        // One overlay owns the keys at a time.
        if self.picker.is_some() {
            self.close_picker();
        }
        // A file previewed by its header is laid out first, so the place kept is in its diff.
        self.finish_preview();
        let places = self.places();
        self.settings_panel = Some(Panel {
            id: self.next_field_id(),
            query: String::new(),
            cursor: Choice::Theme(self.settings.appearance.applied.theme.name.clone()),
            searching: false,
            scroll: ListScroll::default(),
            code_size: self.code_size,
            approved: self.folds.approved,
            places,
            text_code_size: self.code_size,
        });
        self.overlay = Some(Overlay::Settings);
        self.settings_cursor_moved();
    }

    /// Closes the list, dropping a preview; what was applied stays.
    pub(super) fn close_settings(&mut self) {
        self.preview(Preview::default());
        self.settings_panel = None;
        if self.overlay == Some(Overlay::Settings) {
            self.overlay = None;
        }
    }

    /// Keeps the cursor on a shown entry, the first one when a filter took its entry away,
    /// scrolls to it, and previews it.
    pub(super) fn settings_cursor_moved(&mut self) {
        let shown = self.shown_entries();
        let Some(panel) = &mut self.settings_panel else {
            return;
        };
        if !shown.iter().any(|entry| entry.choice == panel.cursor)
            && let Some(first) = shown.first()
        {
            panel.cursor = first.choice.clone();
        }
        let cursor = shown
            .iter()
            .any(|entry| entry.choice == panel.cursor)
            .then(|| panel.cursor.clone());
        if let Some(line) = lines(&shown).iter().position(|line| match line {
            Line::Entry(index) => shown
                .get(*index)
                .is_some_and(|entry| Some(&entry.choice) == cursor.as_ref()),
            Line::Section(_) => false,
        }) {
            panel.scroll.request(Request::Nearest(line));
        }
        let preview = Preview {
            look: cursor.as_ref().and_then(|choice| self.preview_of(choice)),
            code_size: cursor
                .as_ref()
                .and_then(|choice| self.size_preview_of(choice)),
            approved: match cursor {
                Some(Choice::Approved(approved)) => Some(approved),
                _ => None,
            },
        };
        self.preview(preview);
    }

    /// The look an entry previews, when it differs from the applied one.
    fn preview_of(&self, choice: &Choice) -> Option<Look> {
        let applied = &self.settings.appearance.applied;
        let look = match choice {
            Choice::Theme(name) => Look {
                theme: std::sync::Arc::new(self.find_theme(name)?.clone()),
                ..applied.clone()
            },
            Choice::Style(name) => Look {
                style: self.find_style(name)?.clone(),
                ..applied.clone()
            },
            Choice::Density(density) => Look {
                density: *density,
                ..applied.clone()
            },
            Choice::Layout(layout) => Look {
                layout: *layout,
                ..applied.clone()
            },
            Choice::Emphasis(on) => Look {
                token_emphasis: *on,
                ..applied.clone()
            },
            Choice::Approved(_) | Choice::FontSize(_) => return None,
        };
        (look != *applied).then_some(look)
    }

    /// The code size a font size entry previews, when it differs from the one in use.
    fn size_preview_of(&self, choice: &Choice) -> Option<f32> {
        let Choice::FontSize(size) = choice else {
            return None;
        };
        let pixels = points_to_pixels(f32::from(*size));
        ((pixels - self.code_size_in_use()).abs() > 0.01).then_some(pixels)
    }

    /// The code size in use apart from a font size's preview.
    fn code_size_in_use(&self) -> f32 {
        self.settings_panel
            .as_ref()
            .map_or(self.code_size, |panel| panel.code_size)
    }

    /// Whether approved hunks fold apart from a preview.
    fn approved_in_use(&self) -> Approved {
        self.settings_panel
            .as_ref()
            .map_or(self.folds.approved, |panel| panel.approved)
    }

    /// Whether the diff shows a preview rather than what was applied.
    #[must_use]
    pub fn previewing(&self) -> bool {
        self.settings.appearance.preview.is_some()
            || (self.code_size - self.code_size_in_use()).abs() > 0.01
            || self.folds.approved != self.approved_in_use()
    }

    /// Shows `preview` over what is applied and in use, dropping each part that is `None`,
    /// through the presentation transition so a preview's layout, density, size and folds keep
    /// the reviewer's place.
    fn preview(&mut self, preview: Preview) {
        let Preview {
            look,
            code_size,
            approved,
        } = preview;
        let size = code_size.unwrap_or_else(|| self.code_size_in_use());
        let approved = approved.unwrap_or_else(|| self.approved_in_use());
        if self.settings.appearance.preview == look
            && (self.code_size - size).abs() < 0.01
            && self.folds.approved == approved
        {
            return;
        }
        self.settings_transition(|this| {
            this.settings.appearance.preview = look;
            this.code_size = size;
            this.folds.approved = approved;
        });
    }

    /// Changes what shows with `change`: while the list is open, from where the lists were when
    /// it opened, and otherwise from where they are.
    fn settings_transition(&mut self, change: impl FnOnce(&mut Self)) {
        match self.settings_panel.as_ref().map(|panel| panel.places) {
            Some(places) => self.transition_from(places, change),
            None => self.transition(change),
        }
    }

    /// The theme named `name`.
    #[must_use]
    pub fn find_theme(&self, name: &str) -> Option<&Theme> {
        self.settings.themes.iter().find(|theme| theme.name == name)
    }

    fn find_style(&self, name: &str) -> Option<&DiffStyle> {
        self.settings.styles.iter().find(|style| style.name == name)
    }

    /// Whether `choice` is what the review uses now, for the list's marker.
    #[must_use]
    pub fn is_applied(&self, choice: &Choice) -> bool {
        let applied = &self.settings.appearance.applied;
        match choice {
            Choice::Theme(name) => applied.theme.name == *name,
            Choice::Style(name) => applied.style.name == *name,
            Choice::Density(density) => applied.density == *density,
            Choice::Layout(layout) => applied.layout == *layout,
            Choice::Emphasis(on) => applied.token_emphasis == *on,
            Choice::Approved(approved) => self.approved_in_use() == *approved,
            Choice::FontSize(size) => {
                (look::points_of(self.code_size_in_use()) - f32::from(*size)).abs() < 0.5
            }
        }
    }

    pub(super) fn settings_step(&mut self, forward: bool) {
        let shown = self.shown_entries();
        let Some(panel) = &mut self.settings_panel else {
            return;
        };
        let at = shown
            .iter()
            .position(|entry| entry.choice == panel.cursor)
            .unwrap_or(0);
        let next = if forward {
            (at + 1).min(shown.len().saturating_sub(1))
        } else {
            at.saturating_sub(1)
        };
        if let Some(entry) = shown.get(next) {
            panel.cursor = entry.choice.clone();
        }
        self.settings_cursor_moved();
    }

    /// Tab, `/` or a click in the list gives the search field the keys (`true`); Tab or Escape
    /// in the field gives them back to the list, keeping the filter.
    pub(super) fn settings_typing(&mut self, searching: bool) {
        if let Some(panel) = &mut self.settings_panel {
            panel.searching = searching;
        }
    }

    /// A click on the entry that sets `choice`: the cursor moves there and applies it.
    pub fn click_setting(&mut self, choice: Choice) {
        if let Some(panel) = &mut self.settings_panel {
            panel.cursor = choice;
        }
        self.apply_cursor();
    }

    /// Applies the entry under the cursor.
    pub(super) fn apply_cursor(&mut self) {
        let cursor = self
            .settings_panel
            .as_ref()
            .map(|panel| panel.cursor.clone());
        let shown = self.shown_entries();
        if let Some(choice) =
            cursor.filter(|cursor| shown.iter().any(|entry| entry.choice == *cursor))
        {
            self.apply_choice(&choice);
            self.settings_cursor_moved();
        }
    }

    /// Applies `choice` and remembers it.
    fn apply_choice(&mut self, choice: &Choice) {
        let applied = self.settings.appearance.applied.clone();
        let (look, remembered, what) = match choice {
            Choice::Theme(name) => {
                let Some(theme) = self.find_theme(name).cloned() else {
                    return;
                };
                let what = format!("Theme {}", theme.title);
                let look = Look {
                    theme: std::sync::Arc::new(theme),
                    ..applied
                };
                let remembered = vec![Remembered {
                    table: "theme",
                    key: "name",
                    value: name.as_str().into(),
                }];
                (look, remembered, what)
            }
            Choice::Style(name) => {
                if let Some(style) = self.find_style(name).cloned() {
                    self.apply_style(style);
                }
                return;
            }
            Choice::Density(density) => (
                Look {
                    density: *density,
                    ..applied
                },
                vec![Remembered {
                    table: "diff",
                    key: "density",
                    value: density.name().into(),
                }],
                format!("Density {}", density.name()),
            ),
            Choice::Layout(layout) => {
                return self.apply_layout(*layout);
            }
            Choice::Approved(approved) => {
                return self.apply_approved(*approved);
            }
            Choice::Emphasis(on) => (
                Look {
                    token_emphasis: *on,
                    ..applied
                },
                vec![Remembered {
                    table: "diff",
                    key: "token_emphasis",
                    value: (*on).into(),
                }],
                format!("Token emphasis {}", if *on { "on" } else { "off" }),
            ),
            Choice::FontSize(size) => {
                let pixels = points_to_pixels(f32::from(*size));
                if let Some(panel) = &mut self.settings_panel {
                    panel.code_size = pixels;
                }
                self.settings_transition(|this| {
                    this.code_size = pixels;
                    this.default_code_size = pixels;
                });
                self.remember(
                    &[Remembered {
                        table: "font",
                        key: "size",
                        value: i64::from(*size).into(),
                    }],
                    &format!("Code size {size} pt"),
                );
                return;
            }
        };
        self.apply_look(look, &remembered, &what);
    }

    /// Makes `look` the applied look, dropping any preview, and remembers `remembered`.
    pub(super) fn apply_look(&mut self, look: Look, remembered: &[Remembered], what: &str) {
        self.settings_transition(|this| {
            this.settings.appearance.applied = look;
            this.settings.appearance.preview = None;
        });
        self.remember(remembered, what);
    }

    /// Applies the diff style `style` and remembers it.
    pub(super) fn apply_style(&mut self, style: DiffStyle) {
        let what = format!("Diff style {}: {}", style.name, style.summary());
        let remembered = [Remembered {
            table: "diff",
            key: "style",
            value: style.name.as_str().into(),
        }];
        let look = Look {
            style,
            ..self.settings.appearance.applied.clone()
        };
        self.apply_look(look, &remembered, &what);
    }

    /// `|`: the next diff style, plain, bold, soft and then the user's, remembered.
    pub(super) fn next_diff_style(&mut self) {
        let current = self.settings.appearance.applied.style.name.clone();
        if let Some(style) = look::next_style(&self.settings.styles, &current).cloned() {
            self.apply_style(style);
        }
    }

    /// Applies the diff layout `layout` and remembers it; a narrow window still shows it
    /// unified until it is wide enough, and says so.
    pub(super) fn apply_layout(&mut self, layout: DiffLayout) {
        let look = Look {
            layout,
            ..self.settings.appearance.applied.clone()
        };
        let remembered = [Remembered {
            table: "diff",
            key: "layout",
            value: layout.name().into(),
        }];
        let what = match layout {
            DiffLayout::Unified => "Unified diff".to_owned(),
            DiffLayout::SideBySide => "Side by side".to_owned(),
        };
        self.apply_look(look, &remembered, &what);
        if layout == DiffLayout::SideBySide && !self.side_by_side() && self.failed_at.is_none() {
            self.set_status(
                "Side by side, once the diff is wider; it shows unified until then (Ctrl+B or + \
                 give it room).",
            );
        }
    }

    /// Folds approved hunks or shows them, keeping the reviewer's place, and remembers it.
    pub(super) fn apply_approved(&mut self, approved: Approved) {
        if let Some(panel) = &mut self.settings_panel {
            panel.approved = approved;
        }
        self.settings_transition(|this| this.folds.approved = approved);
        let what = match approved {
            Approved::Folded => "Approved hunks folded to their headers",
            Approved::Shown => "Approved hunks shown",
        };
        self.remember(
            &[Remembered {
                table: "diff",
                key: "approved",
                value: approved.name().into(),
            }],
            what,
        );
    }

    /// Writes `remembered` to the configuration and says what was applied, or that it could
    /// not be saved.
    fn remember(&mut self, remembered: &[Remembered], what: &str) {
        let saved = self
            .settings
            .config_path
            .as_deref()
            .map(|path| crate::config::remember(path, remembered));
        match saved {
            Some(Err(err)) => self.fail(format!(
                "{what} applied, could not save: {}",
                error_chain(&err)
            )),
            _ => self.set_status(format!("{what}.")),
        }
    }

    /// `s` in the files and the diff: side by side or unified.
    pub(super) fn toggle_layout(&mut self) {
        let layout = self.settings.appearance.applied.layout.toggled();
        self.apply_layout(layout);
    }
}

/// An error and its causes, as one line.
fn error_chain(err: &dyn std::error::Error) -> String {
    std::iter::successors(Some(err), |err| err.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every word of a query has to match, in any order and case.
    #[test]
    fn search_matches_every_word() {
        let entry = Entry {
            section: "Layout",
            label: "side by side".to_owned(),
            detail: "base on the left, head on the right · s".to_owned(),
            keywords: "split".to_owned(),
            choice: Choice::Layout(DiffLayout::SideBySide),
        };
        assert!(entry.matches(""));
        assert!(entry.matches("Side LAYOUT"));
        assert!(entry.matches("by side"));
        assert!(entry.matches("split"), "keywords count");
        assert!(!entry.matches("head"), "the detail only describes");
        assert!(!entry.matches("side unified"));
    }

    /// Section titles come before the first entry of each section, once.
    #[test]
    fn sections_head_their_entries() {
        let entry = |section, label: &str| Entry {
            section,
            label: label.to_owned(),
            detail: String::new(),
            keywords: String::new(),
            choice: Choice::Emphasis(false),
        };
        let entries = [entry("A", "1"), entry("A", "2"), entry("B", "3")];
        assert_eq!(
            lines(&entries),
            [
                Line::Section("A"),
                Line::Entry(0),
                Line::Entry(1),
                Line::Section("B"),
                Line::Entry(2),
            ]
        );
    }

    #[test]
    fn errors_read_as_one_line() {
        let err = crate::config::RememberError::Write {
            path: "/x/config.toml".into(),
            source: std::io::Error::other("disk full"),
        };
        assert_eq!(error_chain(&err), "cannot write /x/config.toml: disk full");
    }
}
