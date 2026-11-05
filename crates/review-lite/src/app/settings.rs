//! The settings list, opened with `T` or `,`: presets, themes, diff styles, density, layout,
//! token emphasis, approved hunks and font size, keyboard first, with a search over every
//! entry.
//!
//! The list and its search field own the keyboard apart, as two key contexts: the list takes
//! `j`/`k`, Space and `q`, the field takes everything typed, and Tab switches between them.
//! While the cursor rests on any entry, the review shows it as a preview over what the reviewer
//! applied, through the same transition that keeps the reviewer's place; moving off the entry,
//! filtering it out or closing the list in any way drops the preview.
//! The panel takes the files pane's place while it is open, so the diff beside it keeps its
//! exact geometry; when the files pane is hidden or too narrow, it covers the left of the diff
//! instead. It keeps the text size and row height it opened with, so previewing a density or
//! a font size changes the diff beside it and never the panel itself.
//! Applying moves the applied look and writes it to the configuration, a preset all its fields
//! at once; when that write fails, the look stays applied and the status line says it could not
//! be saved.

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Entity, FontWeight, Hsla, MouseButton, ScrollStrategy, Subscription,
    UniformListScrollHandle, Window, actions, div, px, uniform_list,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use gpui_component::{h_flex, v_flex};

use super::keymap::Area;
use super::present::Places;
use super::render::{BAR, GRID, Palette};
use super::{Overlay, ReviewApp, chrome, plural};
use crate::config::{Approved, Density, DiffLayout, Remembered, points_to_pixels};
use crate::look::{self, DiffStyle, FONT_SIZES, Look};
use crate::theme::Theme;

actions!(
    review_lite,
    [
        OpenSettings,
        ToggleLayout,
        ToggleApproved,
        SettingsNext,
        SettingsPrevious,
        SettingsApply,
        SettingsSearch,
        SettingsBrowse,
        SettingsClose,
    ]
);

/// How wide the settings panel is over the diff, when the files pane has no room for it, in
/// logical pixels.
const PANEL_WIDTH: f32 = 440.0;

/// The narrowest files pane the panel takes the place of.
const PANEL_MIN_WIDTH: f32 = 260.0;

/// The narrowest panel that lists each entry's detail beside its label; a narrower one shows
/// only the cursor's, in its footer.
const DETAILS_MIN_WIDTH: f32 = 400.0;

/// The panel's row height as a multiple of its text size.
const PANEL_LINE_HEIGHT: f32 = 1.75;

/// What an entry of the list sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// A named look, by name.
    Preset(String),
    /// A theme, by name.
    Theme(String),
    /// A diff style, by name.
    Style(String),
    Density(Density),
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
    pub keywords: String,
    /// What applying it sets.
    pub choice: Choice,
    /// Why it cannot be applied, for a preset naming a missing theme.
    pub problem: Option<String>,
}

impl Entry {
    /// Whether every word of `query` appears in the entry's section, label or keywords; the
    /// detail only describes, so searching "compact" finds the density, not every preset that
    /// happens to be compact.
    fn matches(&self, query: &str) -> bool {
        let haystack = format!("{} {} {}", self.section, self.label, self.keywords).to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| haystack.contains(word))
    }
}

/// Where an entry's row is and how it draws.
#[derive(Debug, Clone, Copy)]
struct EntryRow {
    /// The row's line in the list.
    position: usize,
    under_cursor: bool,
    row_height: f32,
    /// Whether the panel is wide enough to show the entry's detail beside its label.
    details: bool,
}

/// A line of the list: a section's title, or an entry by its index among the shown entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Section(&'static str),
    Entry(usize),
}

/// The open settings list.
pub struct Panel {
    input: Entity<InputState>,
    /// What the search field holds.
    query: String,
    /// The entry under the cursor, kept by what it sets so filtering keeps it when it can.
    cursor: Choice,
    /// Whether the search field has the keys; otherwise the list has them.
    searching: bool,
    list: UniformListScrollHandle,
    /// The code size in use apart from a font size's preview, in logical pixels.
    code_size: f32,
    /// Whether approved hunks fold apart from a preview.
    approved: Approved,
    /// Where the lists were when the panel opened; every preview starts from there, so one
    /// that moved a list to fit it never moves the place for good.
    places: Places,
    /// The panel's own text size and row height, taken when it opened.
    text_size: f32,
    row_height: f32,
    /// How wide the panel drew last, for QA mode's `state`.
    width: std::cell::Cell<f32>,
    _subscription: Subscription,
}

/// Every entry of the list, in order: presets, themes, then each setting's options.
fn entries(app: &ReviewApp) -> Vec<Entry> {
    let mut entries = Vec::new();
    for preset in &app.settings.presets {
        let (detail, problem) = match &preset.look {
            Ok(look) => (look.summary(), None),
            Err(problem) => (format!("unusable: {problem}"), Some(problem.clone())),
        };
        entries.push(Entry {
            section: "Presets",
            label: preset.name.clone(),
            detail,
            keywords: "preset".to_owned(),
            choice: Choice::Preset(preset.name.clone()),
            problem,
        });
    }
    for theme in &app.settings.themes {
        entries.push(Entry {
            section: "Theme",
            label: theme.title.clone(),
            detail: String::new(),
            keywords: theme.name.clone(),
            choice: Choice::Theme(theme.name.clone()),
            problem: None,
        });
    }
    for style in &app.settings.styles {
        entries.push(Entry {
            section: "Diff style",
            label: style.name.clone(),
            detail: style.summary(),
            keywords: "|".to_owned(),
            choice: Choice::Style(style.name.clone()),
            problem: None,
        });
    }
    let mut option = |section: &'static str, label: &str, detail: &str, choice: Choice| {
        entries.push(Entry {
            section,
            label: label.to_owned(),
            detail: detail.to_owned(),
            keywords: String::new(),
            choice,
            problem: None,
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
    for &size in FONT_SIZES {
        let default = (look::points_of(app.fonts.default_code_size) - f32::from(size)).abs() < 0.5;
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
fn lines(entries: &[Entry]) -> Vec<Line> {
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

impl ReviewApp {
    /// The entries the search shows.
    fn shown_entries(&self) -> Vec<Entry> {
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

    /// Whether the list's search field has the keys.
    pub(super) fn settings_searching(&self) -> bool {
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

    /// Gives the keyboard to the search field or the list; returns whether the list is open.
    pub(super) fn focus_settings(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(panel) = &self.settings_panel else {
            return false;
        };
        if panel.searching {
            panel.input.update(cx, |input, cx| input.focus(window, cx));
        } else {
            window.focus(&self.overlay_focus, cx);
        }
        true
    }

    /// `T` and `,`: opens the list with its cursor on the theme in use.
    pub(super) fn open_settings(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // One overlay owns the keys at a time.
        if self.picker.is_some() {
            self.close_picker(window, cx);
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search settings"));
        let subscription = cx.subscribe_in(&input, window, Self::settings_input_event);
        // A file previewed by its header is laid out first, so the place kept is in its diff.
        self.finish_preview();
        let places = self.places();
        let text_size = self.fonts.ui_size();
        self.settings_panel = Some(Panel {
            input,
            query: String::new(),
            cursor: Choice::Theme(self.settings.appearance.applied.theme.name.clone()),
            searching: false,
            list: UniformListScrollHandle::new(),
            code_size: self.fonts.code_size,
            approved: self.folds.approved,
            places,
            text_size,
            row_height: (text_size * PANEL_LINE_HEIGHT).round(),
            width: std::cell::Cell::new(PANEL_WIDTH),
            _subscription: subscription,
        });
        self.overlay = Some(Overlay::Settings);
        self.focus_mode(window, cx);
        self.settings_cursor_moved();
        cx.notify();
    }

    /// Closes the list, dropping a preview; what was applied stays.
    pub(super) fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview(Preview::default());
        self.settings_panel = None;
        if self.overlay == Some(Overlay::Settings) {
            self.overlay = None;
        }
        self.focus_mode(window, cx);
        cx.notify();
    }

    fn settings_close(&mut self, _: &SettingsClose, window: &mut Window, cx: &mut Context<Self>) {
        self.close_settings(window, cx);
    }

    fn settings_input_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                if let Some(panel) = &mut self.settings_panel {
                    panel.query = query;
                }
                self.settings_cursor_moved();
                cx.notify();
            }
            // The field passes Enter on to the search context's binding, which applies.
            InputEvent::PressEnter { .. } | InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// Keeps the cursor on a shown entry, the first one when a filter took its entry away,
    /// scrolls to it, and previews it.
    fn settings_cursor_moved(&mut self) {
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
            panel.list.scroll_to_item(line, ScrollStrategy::Nearest);
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
            Choice::Preset(name) => self.preset_look(name)?.ok()?,
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
            .map_or(self.fonts.code_size, |panel| panel.code_size)
    }

    /// Whether approved hunks fold apart from a preview.
    fn approved_in_use(&self) -> Approved {
        self.settings_panel
            .as_ref()
            .map_or(self.folds.approved, |panel| panel.approved)
    }

    /// Whether the diff shows a preview rather than what was applied.
    pub(super) fn previewing(&self) -> bool {
        self.settings.appearance.preview.is_some()
            || (self.fonts.code_size - self.code_size_in_use()).abs() > 0.01
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
            && (self.fonts.code_size - size).abs() < 0.01
            && self.folds.approved == approved
        {
            return;
        }
        self.settings_transition(|this| {
            this.settings.appearance.preview = look;
            this.fonts.code_size = size;
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

    fn find_theme(&self, name: &str) -> Option<&Theme> {
        self.settings.themes.iter().find(|theme| theme.name == name)
    }

    fn find_style(&self, name: &str) -> Option<&DiffStyle> {
        self.settings.styles.iter().find(|style| style.name == name)
    }

    /// The resolved look of the preset `name`, or why it cannot be used.
    fn preset_look(&self, name: &str) -> Option<Result<Look, String>> {
        self.settings
            .presets
            .iter()
            .find(|preset| preset.name == name)
            .map(|preset| preset.look.clone())
    }

    /// The preset whose look is the applied one, if any.
    fn applied_preset(&self) -> Option<&str> {
        let applied = &self.settings.appearance.applied;
        self.settings
            .presets
            .iter()
            .find(|preset| preset.look.as_ref().is_ok_and(|look| look == applied))
            .map(|preset| preset.name.as_str())
    }

    /// Whether `choice` is what the review uses now, for the list's marker.
    fn is_applied(&self, choice: &Choice) -> bool {
        let applied = &self.settings.appearance.applied;
        match choice {
            Choice::Preset(name) => {
                matches!(self.preset_look(name), Some(Ok(look)) if look == *applied)
            }
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

    fn settings_step(&mut self, forward: bool, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    fn settings_next(&mut self, _: &SettingsNext, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_step(true, cx);
    }

    fn settings_previous(&mut self, _: &SettingsPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.settings_step(false, cx);
    }

    fn settings_apply(&mut self, _: &SettingsApply, _: &mut Window, cx: &mut Context<Self>) {
        self.apply_cursor(cx);
    }

    /// Tab or `/` in the list: the search field takes the keys.
    fn settings_search(&mut self, _: &SettingsSearch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.settings_panel {
            panel.searching = true;
        }
        self.focus_mode(window, cx);
        cx.notify();
    }

    /// Tab or Escape in the search field: the list takes the keys, keeping the filter.
    fn settings_browse(&mut self, _: &SettingsBrowse, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = &mut self.settings_panel {
            panel.searching = false;
        }
        self.focus_mode(window, cx);
        cx.notify();
    }

    /// Applies the entry under the cursor.
    fn apply_cursor(&mut self, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    /// Applies `choice` and remembers it.
    fn apply_choice(&mut self, choice: &Choice) {
        let applied = self.settings.appearance.applied.clone();
        let (look, remembered, what) = match choice {
            Choice::Preset(name) => match self.preset_look(name) {
                Some(Ok(look)) => {
                    let what = format!("Preset {name}: {}", look.summary());
                    let remembered = look.remembered();
                    (look, remembered, what)
                }
                Some(Err(problem)) => {
                    self.fail(format!("Preset {name} cannot be used: {problem}."));
                    return;
                }
                None => return,
            },
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
                    this.fonts.code_size = pixels;
                    this.fonts.default_code_size = pixels;
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

    /// `zA` in the files and the diff: approved hunks folded or shown.
    pub(super) fn toggle_approved(
        &mut self,
        _: &ToggleApproved,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_approved(self.folds.approved.toggled());
        cx.notify();
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
    pub(super) fn toggle_layout(
        &mut self,
        _: &ToggleLayout,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout = self.settings.appearance.applied.layout.toggled();
        self.apply_layout(layout);
        cx.notify();
    }

    /// The settings overlay, when the list is open: the panel over the files, leaving the diff
    /// in view for previews, and a backdrop that closes it when clicked.
    pub(super) fn render_settings(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self.settings_panel.as_ref()?;
        let width = self.settings_width(window);
        panel.width.set(width);
        let colors = self.palette();
        let shown = self.shown_entries();
        let total = entries(self).len();
        let lines = lines(&shown);
        let row_height = panel.row_height;
        let top = if self.client_decorated {
            chrome::HEIGHT
        } else {
            0.0
        };
        let search = Self::render_settings_search(panel, shown.len(), total, &colors);
        let list = uniform_list(
            "settings",
            lines.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                this.render_settings_lines(range, width >= DETAILS_MIN_WIDTH, cx)
            }),
        )
        .track_scroll(&panel.list)
        .size_full();
        let empty = shown.is_empty().then(|| {
            div()
                .px(px(GRID * 3.0))
                .py(px(GRID * 2.0))
                .text_color(colors.faint)
                .child("No setting matches; Esc goes back to the list.")
        });
        let body = v_flex()
            .absolute()
            .left_0()
            .top(px(top))
            .bottom(px(2.0 * (BAR - GRID)))
            .w(px(width))
            .text_size(px(panel.text_size))
            .bg(colors.surface)
            .border_r_1()
            .border_color(colors.hairline)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.render_settings_title(&colors))
            .child(
                search.capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                    // A click into the field gives it the keys, as Tab does.
                    if let Some(panel) = &mut this.settings_panel {
                        panel.searching = true;
                    }
                    this.focus_mode(window, cx);
                    cx.notify();
                })),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .line_height(px(row_height))
                    .child(list)
                    .child(Scrollbar::vertical(&panel.list).mode(ScrollbarMode::Always))
                    .children(empty),
            )
            .child(self.render_settings_footer(&shown, &colors));
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .key_context(self.settings_area().context())
                .track_focus(&self.overlay_focus)
                .on_action(cx.listener(Self::settings_next))
                .on_action(cx.listener(Self::settings_previous))
                .on_action(cx.listener(Self::settings_apply))
                .on_action(cx.listener(Self::settings_search))
                .on_action(cx.listener(Self::settings_browse))
                .on_action(cx.listener(Self::settings_close))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        // The backdrop would take the focus after this listener and keep it
                        // once it is gone, leaving no key context; the mode takes it instead.
                        window.prevent_default();
                        this.close_settings(window, cx);
                    }),
                )
                .child(body)
                .into_any_element(),
        )
    }

    /// The panel's width: the files pane's, whose place it takes so the diff keeps its own, or
    /// its own over the diff when the files pane is hidden or too narrow.
    fn settings_width(&self, window: &Window) -> f32 {
        let files = self.shown_sidebar_width(window);
        if files >= PANEL_MIN_WIDTH {
            files
        } else {
            PANEL_WIDTH
        }
    }

    /// The footer: what the entry under the cursor does, and whether it shows as a preview.
    fn render_settings_footer(&self, shown: &[Entry], colors: &Palette) -> gpui::Div {
        let entry = self
            .settings_panel
            .as_ref()
            .and_then(|panel| shown.iter().find(|entry| entry.choice == panel.cursor));
        let detail = entry.map(|entry| {
            let (text, color) = match &entry.problem {
                Some(problem) => (format!("unusable: {problem}"), colors.error),
                None => (entry.detail.clone(), colors.faint),
            };
            div().min_w_0().truncate().text_color(color).child(text)
        });
        h_flex()
            .h(px(BAR))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .border_t_1()
            .border_color(colors.hairline)
            .children(
                self.previewing()
                    .then(|| div().flex_none().text_color(colors.accent).child("preview")),
            )
            .children(detail)
    }

    /// The panel's title row: its name, and the preset whose look is applied.
    fn render_settings_title(&self, colors: &Palette) -> gpui::Div {
        h_flex()
            .h(px(BAR))
            .flex_none()
            .px(px(GRID * 3.0))
            .border_b_1()
            .border_color(colors.hairline)
            .gap(px(GRID * 3.0))
            .child(
                div()
                    .flex_none()
                    .text_color(colors.text)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Settings"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .text_color(colors.faint)
                    .child(
                        self.applied_preset().map_or_else(
                            || "custom look".to_owned(),
                            |name| format!("preset {name}"),
                        ),
                    ),
            )
    }

    /// The search row: the field, accented while it has the keys, and how many entries show.
    fn render_settings_search(
        panel: &Panel,
        shown: usize,
        total: usize,
        colors: &Palette,
    ) -> gpui::Div {
        let focused = panel.searching;
        let count = if panel.query.is_empty() {
            plural(total, "setting")
        } else {
            format!("{shown}/{total}")
        };
        let accent = if focused {
            colors.accent
        } else {
            colors.hairline
        };
        h_flex()
            .h(px(BAR))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .border_b_1()
            .border_color(accent)
            .child(
                div()
                    .text_color(if focused { colors.accent } else { colors.faint })
                    .child("/"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&panel.input).appearance(false)),
            )
            .child(div().flex_none().text_color(colors.faint).child(count))
    }

    /// The list's lines in `range`: section titles, and entries with their marker, label,
    /// swatch and, when `details`, their detail.
    fn render_settings_lines(
        &mut self,
        range: std::ops::Range<usize>,
        details: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = self.palette();
        let Some((row_height, cursor)) = self
            .settings_panel
            .as_ref()
            .map(|panel| (panel.row_height, panel.cursor.clone()))
        else {
            return Vec::new();
        };
        let shown = self.shown_entries();
        let lines = lines(&shown);
        let cursor = Some(cursor);
        range
            .filter_map(|position| {
                let line = *lines.get(position)?;
                let element = match line {
                    // A hairline above each section but the first sets the groups apart.
                    Line::Section(title) => div()
                        .h(px(row_height))
                        .w_full()
                        .px(px(GRID * 3.0))
                        .flex()
                        .items_end()
                        .pb(px(GRID))
                        .when(position > 0, |title| {
                            title.border_t_1().border_color(colors.hairline)
                        })
                        .text_color(colors.faint)
                        .child(title.to_uppercase())
                        .into_any_element(),
                    Line::Entry(index) => {
                        let entry = shown.get(index)?;
                        let under_cursor = cursor.as_ref() == Some(&entry.choice);
                        let row = EntryRow {
                            position,
                            under_cursor,
                            row_height,
                            details,
                        };
                        self.render_entry(entry, row, &colors, cx)
                    }
                };
                Some(element)
            })
            .collect()
    }

    fn render_entry(
        &self,
        entry: &Entry,
        row: EntryRow,
        colors: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let EntryRow {
            position,
            under_cursor,
            row_height,
            details,
        } = row;
        let applied = self.is_applied(&entry.choice);
        let swatch = match &entry.choice {
            Choice::Theme(name) => self.find_theme(name).map(swatch),
            Choice::Preset(name) => match self.preset_look(name) {
                Some(Ok(look)) => Some(swatch(&look.theme)),
                Some(Err(_)) | None => None,
            },
            Choice::Style(_)
            | Choice::Density(_)
            | Choice::Layout(_)
            | Choice::Emphasis(_)
            | Choice::Approved(_)
            | Choice::FontSize(_) => None,
        };
        let choice = entry.choice.clone();
        let label = div()
            .min_w_0()
            .truncate()
            .text_color(if entry.problem.is_some() {
                colors.faint
            } else if applied || under_cursor {
                colors.text
            } else {
                colors.dim
            })
            .child(entry.label.clone());
        // With details, labels share one column so swatches and details line up down the
        // list; without, the label takes the room and the swatch sits at the edge.
        let label = if details {
            let text_size = self
                .settings_panel
                .as_ref()
                .map_or(self.fonts.ui_size(), |panel| panel.text_size);
            label.w(px(text_size * 0.6 * 20.0)).flex_none()
        } else {
            label.flex_1()
        };
        h_flex()
            .id(("setting", position))
            .w_full()
            .h(px(row_height))
            .pr(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .cursor_pointer()
            .when(under_cursor, |row| row.bg(colors.raised))
            .hover(|row| row.bg(colors.hairline))
            .child(
                div()
                    .w(px(3.0))
                    .h_full()
                    .flex_none()
                    .when(under_cursor, |bar| bar.bg(colors.accent)),
            )
            .child(
                div()
                    .w(px(GRID * 3.0))
                    .flex_none()
                    .text_color(colors.accepted)
                    .child(if applied { "●" } else { "" }),
            )
            .child(label)
            .children(swatch)
            .children(details.then(|| {
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(if entry.problem.is_some() {
                        colors.error
                    } else {
                        colors.faint
                    })
                    .child(entry.detail.clone())
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(panel) = &mut this.settings_panel {
                    panel.cursor = choice.clone();
                }
                this.apply_cursor(cx);
            }))
            .into_any_element()
    }
}

impl ReviewApp {
    /// The open settings list as QA mode's `state` reports it, or `null`.
    pub(super) fn qa_settings(&self) -> serde_json::Value {
        let Some(panel) = &self.settings_panel else {
            return serde_json::Value::Null;
        };
        let shown = self.shown_entries();
        // Where the cursor's row is on screen, to check that previews never move the panel.
        let cursor_line = lines(&shown).iter().position(|line| match line {
            Line::Entry(index) => shown
                .get(*index)
                .is_some_and(|entry| entry.choice == panel.cursor),
            Line::Section(_) => false,
        });
        let viewport = panel.list.viewport_bounds();
        let scrolled = f32::from(ScrollbarHandle::offset(&panel.list).y);
        let cursor_top = cursor_line.map(|line| {
            f32::from(viewport.top()) + super::scroll::float(line) * panel.row_height + scrolled
        });
        serde_json::json!({
            "geometry": {
                "width": panel.width.get(),
                "text_size": panel.text_size,
                "row_height": panel.row_height,
                "list_top": f32::from(viewport.top()),
                "cursor_top": cursor_top,
            },
            "query": panel.query,
            "searching": panel.searching,
            "cursor": shown
                .iter()
                .find(|entry| entry.choice == panel.cursor)
                .map(|entry| format!("{}: {}", entry.section, entry.label)),
            "entries": shown.len(),
        })
    }
}

/// Four squares of a theme's colors: its background, removed and added lines, and accent.
fn swatch(theme: &Theme) -> gpui::Div {
    let square = |color: Hsla| div().size(px(GRID * 2.5)).flex_none().bg(color);
    h_flex()
        .flex_none()
        .border_1()
        .border_color(theme.ui.hairline)
        .child(square(theme.ui.background))
        .child(square(theme.diff.removed_line))
        .child(square(theme.diff.added_line))
        .child(square(theme.ui.accent))
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
            problem: None,
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
            problem: None,
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
