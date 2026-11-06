//! The action registry: every action with its default keys, where it applies, its help text and
//! whether the bottom bar shows it. Key dispatch, the `[keys]` overrides, the `?` cheat sheet and
//! the hint bar all read this one table.
//!
//! The defaults follow lazygit for the review flow (space approves, `0` focuses the diff, `+`/`_`
//! change the screen mode) and vim for moving (`j`/`k`, `g g`/`G`, `Ctrl+D`/`Ctrl+U`, `w`/`b`,
//! `/`, `n`/`N`, `z` folds).

use std::collections::BTreeMap;

use crate::keys::Keystroke;

/// Where a binding applies: a mode, which owns the keyboard while it is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Area {
    /// The file tree.
    Tree,
    /// One file's diff, moving between hunks.
    Diff,
    /// One file in full, read like a vim buffer.
    Buffer,
    /// The find bar of the buffer.
    Find,
    /// The cheat sheet or the pull request overview.
    Overlay,
    /// A picker (the file finder or the content search) while its query takes typing.
    Picker,
    /// A picker while its list takes the keys, as vim's normal mode.
    PickerList,
    /// The settings list while its entries take the keys.
    SettingsList,
    /// The settings list while its search field takes typing; only keys that type nothing.
    SettingsSearch,
    /// Anywhere in the window; only keys with modifiers, so typing never triggers them.
    Window,
}

impl Area {
    /// Every area.
    pub const ALL: &[Self] = &[
        Self::Tree,
        Self::Diff,
        Self::Buffer,
        Self::Find,
        Self::Overlay,
        Self::Picker,
        Self::PickerList,
        Self::SettingsList,
        Self::SettingsSearch,
        Self::Window,
    ];

    /// The area whose gpui key context is `context`.
    #[must_use]
    pub fn from_context(context: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|area| area.context() == context)
    }

    /// The gpui key context of the area.
    #[must_use]
    pub fn context(self) -> &'static str {
        match self {
            Self::Tree => "Tree",
            Self::Diff => "Diff",
            Self::Buffer => "Buffer",
            Self::Find => "FindBar",
            Self::Overlay => "Overlay",
            Self::Picker => "Picker",
            Self::PickerList => "PickerList",
            Self::SettingsList => "SettingsList",
            Self::SettingsSearch => "SettingsSearch",
            Self::Window => "ReviewWindow",
        }
    }
}

/// One action's keys in one area, with what the cheat sheet and the bottom bar say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// The action's name, as [`crate::model::Action::name`] and `[keys]` name it.
    pub action: &'static str,
    /// Its keystrokes, each a keystroke sequence such as `g d` or `ctrl-shift-e`, in the grammar
    /// of [`Keystroke::parse`].
    pub keys: Vec<String>,
    /// Where it applies.
    pub area: Area,
    /// The cheat sheet's section.
    pub group: &'static str,
    /// What it does, for the cheat sheet; empty to leave it out there.
    pub help: &'static str,
    /// The bottom bar's label and rank, lower first; `None` keeps it off the bar.
    pub hint: Option<(u8, &'static str)>,
    /// An action whose first key the bar shows together with this one's, as in `j/k`.
    pub pair: Option<&'static str>,
}

/// One default: action, keys, areas, cheat-sheet group and help, bar rank and label, pair.
type Default = (
    &'static str,
    &'static [&'static str],
    &'static [Area],
    &'static str,
    &'static str,
    Option<(u8, &'static str)>,
    Option<&'static str>,
);

const MODES: &[Area] = &[Area::Tree, Area::Diff, Area::Buffer];
const TREE: &[Area] = &[Area::Tree];
const DIFF: &[Area] = &[Area::Diff];
const BUFFER: &[Area] = &[Area::Buffer];
const REVIEW: &[Area] = &[Area::Diff, Area::Buffer];
const WINDOW: &[Area] = &[Area::Window];
const PICKER: &[Area] = &[Area::Picker];
const PICKER_LIST: &[Area] = &[Area::PickerList];
const PICKERS: &[Area] = &[Area::Picker, Area::PickerList];
const SETTINGS_LIST: &[Area] = &[Area::SettingsList];
const SETTINGS_SEARCH: &[Area] = &[Area::SettingsSearch];

/// The default bindings, in cheat-sheet order.
const DEFAULTS: &[Default] = &[
    // The file tree.
    (
        "TreeDown",
        &["j", "down"],
        TREE,
        "Files",
        "next file",
        Some((1, "move")),
        Some("TreeUp"),
    ),
    (
        "TreeUp",
        &["k", "up"],
        TREE,
        "Files",
        "previous file",
        None,
        None,
    ),
    (
        "TreeCollapse",
        &["h", "left"],
        TREE,
        "Files",
        "fold folder · parent",
        None,
        None,
    ),
    (
        "TreeExpand",
        &["l", "right"],
        TREE,
        "Files",
        "unfold folder · open diff",
        None,
        None,
    ),
    (
        "Activate",
        &["enter"],
        TREE,
        "Files",
        "fold folder · open diff",
        Some((3, "open")),
        None,
    ),
    (
        "FocusDiff",
        &["0"],
        TREE,
        "Files",
        "focus the diff",
        Some((4, "diff")),
        None,
    ),
    (
        "ClearFilter",
        &["escape"],
        TREE,
        "Files",
        "clear the filter",
        None,
        None,
    ),
    (
        "CloseAllFolds",
        &["z shift-m"],
        TREE,
        "Files",
        "fold all folders",
        None,
        None,
    ),
    (
        "OpenAllFolds",
        &["z shift-r"],
        TREE,
        "Files",
        "unfold all folders",
        None,
        None,
    ),
    // One file's diff.
    (
        "NextHunk",
        &["j", "down"],
        DIFF,
        "Diff",
        "next hunk",
        Some((1, "hunk")),
        Some("PreviousHunk"),
    ),
    (
        "PreviousHunk",
        &["k", "up"],
        DIFF,
        "Diff",
        "previous hunk",
        None,
        None,
    ),
    (
        "FirstHunk",
        &["g g", "home"],
        DIFF,
        "Diff",
        "first hunk",
        None,
        None,
    ),
    (
        "LastHunk",
        &["shift-g", "end"],
        DIFF,
        "Diff",
        "last hunk",
        None,
        None,
    ),
    (
        "HalfPageDown",
        &["ctrl-d", "pagedown"],
        &[Area::Diff, Area::Buffer],
        "Diff",
        "scroll down",
        None,
        None,
    ),
    (
        "HalfPageUp",
        &["ctrl-u", "pageup"],
        &[Area::Diff, Area::Buffer],
        "Diff",
        "scroll up",
        None,
        None,
    ),
    (
        "PanRight",
        &["shift-right", "z shift-l"],
        REVIEW,
        "Diff",
        "pan the code right",
        None,
        None,
    ),
    (
        "PanLeft",
        &["shift-left", "z shift-h"],
        REVIEW,
        "Diff",
        "pan the code left",
        None,
        None,
    ),
    (
        "ToggleFold",
        &["z a"],
        DIFF,
        "Diff",
        "fold or unfold the hunk",
        Some((6, "fold")),
        None,
    ),
    (
        "Activate",
        &["enter"],
        DIFF,
        "Diff",
        "follow move · expand",
        None,
        None,
    ),
    (
        "NextFile",
        &["n"],
        DIFF,
        "Diff",
        "next file to review",
        Some((7, "next file")),
        None,
    ),
    (
        "FocusTree",
        &["escape", "h", "left"],
        DIFF,
        "Diff",
        "back to the files",
        Some((8, "files")),
        None,
    ),
    (
        "ToggleBuffer",
        &["="],
        REVIEW,
        "Diff",
        "whole file · back to the diff",
        Some((5, "file")),
        None,
    ),
    // Reviewing, in the tree and the diff alike.
    (
        "ToggleApprove",
        &["space"],
        MODES,
        "Review",
        "approve or unapprove",
        Some((2, "approve")),
        None,
    ),
    (
        "ToggleFlag",
        &["!"],
        MODES,
        "Review",
        "flag as needs review",
        Some((4, "flag")),
        None,
    ),
    (
        "ResetReview",
        &["u"],
        &[Area::Tree, Area::Diff],
        "Review",
        "back to unreviewed",
        None,
        None,
    ),
    (
        "Refresh",
        &["shift-r"],
        MODES,
        "Review",
        "fetch · switch to a new revision",
        None,
        None,
    ),
    (
        "ToggleTrust",
        &["ctrl-t"],
        WINDOW,
        "Review",
        "trust or revoke trust",
        None,
        None,
    ),
    (
        "YankUnitId",
        &["y i"],
        DIFF,
        "Review",
        "copy unit ID",
        None,
        None,
    ),
    // The whole file as a buffer.
    (
        "CursorDown",
        &["j", "down"],
        BUFFER,
        "File",
        "next line",
        Some((1, "move")),
        Some("CursorUp"),
    ),
    (
        "CursorUp",
        &["k", "up"],
        BUFFER,
        "File",
        "previous line",
        None,
        None,
    ),
    (
        "NextWord",
        &["w", "l", "right"],
        BUFFER,
        "File",
        "next word",
        None,
        None,
    ),
    (
        "PreviousWord",
        &["b", "h", "left"],
        BUFFER,
        "File",
        "previous word",
        None,
        None,
    ),
    (
        "BufferTop",
        &["g g", "home"],
        BUFFER,
        "File",
        "first line",
        None,
        None,
    ),
    (
        "BufferBottom",
        &["shift-g", "end"],
        BUFFER,
        "File",
        "last line",
        None,
        None,
    ),
    (
        "OpenFind",
        &["/"],
        MODES,
        "Search",
        "filter files · find in the diff or file",
        Some((3, "find")),
        None,
    ),
    (
        "FindNext",
        &["n"],
        BUFFER,
        "Search",
        "next match",
        None,
        None,
    ),
    (
        "FindPrevious",
        &["shift-n"],
        REVIEW,
        "Search",
        "previous match",
        None,
        None,
    ),
    (
        "FindFiles",
        &["ctrl-p"],
        MODES,
        "Search",
        "find a file by name",
        Some((10, "files")),
        None,
    ),
    (
        "SearchContent",
        &["ctrl-g"],
        MODES,
        "Search",
        "search file contents",
        Some((11, "grep")),
        None,
    ),
    (
        "GoToDefinition",
        &["g d", "f12"],
        BUFFER,
        "File",
        "go to definition",
        Some((4, "definition")),
        None,
    ),
    (
        "JumpBack",
        &["ctrl-o"],
        BUFFER,
        "File",
        "back from a definition",
        None,
        None,
    ),
    (
        "Dismiss",
        &["escape"],
        BUFFER,
        "File",
        "close find · back",
        Some((8, "back")),
        None,
    ),
    (
        "CloseFind",
        &["escape"],
        &[Area::Find],
        "File",
        "",
        Some((2, "cancel")),
        None,
    ),
    // Views and the window.
    (
        "ToggleOverview",
        &["i"],
        MODES,
        "View",
        "pull request overview",
        None,
        None,
    ),
    (
        "ToggleDiffStyle",
        &["|"],
        MODES,
        "View",
        "next diff style: plain, bold, soft, then yours",
        // The bar names the style in use instead, such as `| soft`.
        Some((12, "style")),
        None,
    ),
    (
        "ToggleLayout",
        &["s"],
        &[Area::Tree, Area::Diff],
        "View",
        "side by side or unified",
        Some((13, "split")),
        None,
    ),
    (
        "ToggleApproved",
        &["z shift-a"],
        &[Area::Tree, Area::Diff],
        "View",
        "show or fold approved hunks",
        None,
        None,
    ),
    (
        "OpenSettings",
        &["shift-t", ","],
        MODES,
        "View",
        "settings: themes, diff styles, density, layout, font size",
        Some((14, "settings")),
        None,
    ),
    (
        "ScreenNext",
        &["+"],
        MODES,
        "View",
        "wider diff",
        None,
        None,
    ),
    (
        "ScreenPrevious",
        &["_"],
        MODES,
        "View",
        "back to the split",
        None,
        None,
    ),
    (
        "ToggleSidebar",
        &["ctrl-b"],
        WINDOW,
        "View",
        "show or hide the files",
        None,
        None,
    ),
    (
        "ToggleHelp",
        &["?"],
        MODES,
        "View",
        "all keys",
        Some((u8::MAX, "keys")),
        None,
    ),
    (
        "CloseOverlay",
        &["escape", "q", "?", "i"],
        &[Area::Overlay],
        "View",
        "",
        Some((1, "close")),
        None,
    ),
    (
        "SheetDown",
        &["j", "down"],
        &[Area::Overlay],
        "View",
        "",
        Some((2, "scroll")),
        Some("SheetUp"),
    ),
    (
        "SheetUp",
        &["k", "up"],
        &[Area::Overlay],
        "View",
        "",
        None,
        None,
    ),
    (
        "SheetPageDown",
        &["ctrl-d", "pagedown"],
        &[Area::Overlay],
        "View",
        "",
        None,
        None,
    ),
    (
        "SheetPageUp",
        &["ctrl-u", "pageup"],
        &[Area::Overlay],
        "View",
        "",
        None,
        None,
    ),
    // The pickers: the file finder and the content search.
    (
        "PickerNext",
        &["ctrl-n", "down"],
        PICKER,
        "Pickers",
        "next entry",
        Some((1, "move")),
        Some("PickerPrevious"),
    ),
    (
        "PickerPrevious",
        &["ctrl-p", "up"],
        PICKER,
        "Pickers",
        "previous entry",
        None,
        None,
    ),
    (
        "PickerNext",
        &["j", "down", "ctrl-n"],
        PICKER_LIST,
        "Pickers",
        "",
        Some((1, "move")),
        Some("PickerPrevious"),
    ),
    (
        "PickerPrevious",
        &["k", "up", "ctrl-p"],
        PICKER_LIST,
        "Pickers",
        "",
        None,
        None,
    ),
    (
        "PickerOpen",
        &["enter"],
        PICKERS,
        "Pickers",
        "open the entry",
        Some((2, "open")),
        None,
    ),
    (
        "PickerScope",
        &["tab"],
        PICKERS,
        "Pickers",
        "changed files · whole tree",
        Some((3, "scope")),
        None,
    ),
    (
        "PickerBrowse",
        &["escape"],
        PICKER,
        "Pickers",
        "leave the query for the list",
        Some((4, "list")),
        None,
    ),
    (
        "PickerEdit",
        &["i", "/"],
        PICKER_LIST,
        "Pickers",
        "back to the query",
        Some((4, "type")),
        None,
    ),
    (
        "PickerClose",
        &["escape", "q"],
        PICKER_LIST,
        "Pickers",
        "close the picker",
        Some((5, "close")),
        None,
    ),
    // The settings list, whose entries and search field take the keys apart.
    (
        "SettingsNext",
        &["j", "down", "ctrl-n"],
        SETTINGS_LIST,
        "Settings",
        "next entry, previewing it",
        Some((1, "move")),
        Some("SettingsPrevious"),
    ),
    (
        "SettingsPrevious",
        &["k", "up", "ctrl-p"],
        SETTINGS_LIST,
        "Settings",
        "previous entry",
        None,
        None,
    ),
    (
        "SettingsNext",
        &["ctrl-n", "down", "ctrl-j"],
        SETTINGS_SEARCH,
        "Settings",
        "",
        Some((1, "move")),
        Some("SettingsPrevious"),
    ),
    (
        "SettingsPrevious",
        &["ctrl-p", "up", "ctrl-k"],
        SETTINGS_SEARCH,
        "Settings",
        "",
        None,
        None,
    ),
    (
        "SettingsApply",
        &["space", "enter"],
        SETTINGS_LIST,
        "Settings",
        "apply and remember the entry",
        Some((2, "apply")),
        None,
    ),
    (
        "SettingsApply",
        &["enter"],
        SETTINGS_SEARCH,
        "Settings",
        "",
        Some((2, "apply")),
        None,
    ),
    (
        "SettingsSearch",
        &["/", "tab"],
        SETTINGS_LIST,
        "Settings",
        "search the settings",
        Some((3, "search")),
        None,
    ),
    (
        "SettingsBrowse",
        &["escape", "tab"],
        SETTINGS_SEARCH,
        "Settings",
        "back to the list, keeping the search",
        Some((3, "list")),
        None,
    ),
    (
        "SettingsClose",
        &["escape", "q"],
        SETTINGS_LIST,
        "Settings",
        "close, keeping what was applied",
        Some((4, "close")),
        None,
    ),
    (
        "Copy",
        &["ctrl-c", "ctrl-insert"],
        REVIEW,
        "View",
        "copy selection",
        None,
        None,
    ),
    (
        "ZoomIn",
        &["ctrl-=", "ctrl-+", "ctrl-shift-="],
        WINDOW,
        "Window",
        "zoom in",
        None,
        None,
    ),
    (
        "ZoomOut",
        &["ctrl--"],
        WINDOW,
        "Window",
        "zoom out",
        None,
        None,
    ),
    (
        "ZoomReset",
        &["ctrl-0"],
        WINDOW,
        "Window",
        "reset zoom",
        None,
        None,
    ),
    (
        "ToggleFullscreen",
        &["f11"],
        WINDOW,
        "Window",
        "full screen",
        None,
        None,
    ),
    (
        "CloseWindow",
        &["ctrl-w"],
        WINDOW,
        "Window",
        "close window",
        None,
        None,
    ),
    (
        "Quit",
        &["q"],
        &[Area::Tree, Area::Diff],
        "Window",
        "quit",
        None,
        None,
    ),
    ("Quit", &["ctrl-q"], WINDOW, "Window", "", None, None),
];

/// The keymap after the configuration's `[keys]` overrides, each of which replaces an action's
/// default keys in every area, with an empty list unbinding it. Returns the problems found, such
/// as unknown action names, alongside.
#[must_use]
pub fn keymap(overrides: &BTreeMap<String, Vec<String>>) -> (Vec<Binding>, Vec<String>) {
    let mut problems: Vec<String> = overrides
        .keys()
        .filter(|name| !DEFAULTS.iter().any(|(action, ..)| action == name))
        .map(|name| format!("[keys] names an unknown action {name:?}"))
        .collect();
    let mut bindings = Vec::new();
    for &(action, keys, areas, group, help, hint, pair) in DEFAULTS {
        for &area in areas {
            bindings.push(Binding {
                action,
                keys: overrides
                    .get(action)
                    .cloned()
                    .unwrap_or_else(|| keys.iter().map(|&key| key.to_owned()).collect()),
                area,
                group,
                // The cheat sheet lists an action once, under its first area.
                help: if areas.first() == Some(&area) {
                    help
                } else {
                    ""
                },
                hint,
                pair,
            });
        }
    }
    let mut reported = std::collections::HashSet::new();
    for binding in &bindings {
        for key in &binding.keys {
            let valid = !key.trim().is_empty() && Keystroke::parse_sequence(key).is_ok();
            if !valid && reported.insert((binding.action, key.clone())) {
                problems.push(format!(
                    "[keys] {}: {key:?} is not a keystroke",
                    binding.action
                ));
            }
        }
    }
    (bindings, problems)
}

/// One hint of the bottom bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// The action it names.
    pub action: &'static str,
    /// The keys, as the bar shows them, such as `j/k`.
    pub keys: String,
    /// What the bar calls the action, such as `approve`.
    pub label: &'static str,
    /// Lower ranks come first and are the last to make way when the bar is narrow.
    pub rank: u8,
    /// Whether the action can do something now; the bar dims it otherwise, keeping its place.
    pub available: bool,
}

/// The bottom bar's hints for `area`: every action with a hint, its keys and label, in rank
/// order, each noting whether `available` allows it now.
#[must_use]
pub fn hints(bindings: &[Binding], area: Area, available: impl Fn(&str) -> bool) -> Vec<Hint> {
    let first_key = |action: &str| {
        bindings
            .iter()
            .find(|binding| binding.area == area && binding.action == action)
            .and_then(|binding| binding.keys.first())
            .map(|key| display(key))
    };
    let mut shown: Vec<Hint> = bindings
        .iter()
        .filter(|binding| binding.area == area)
        .filter_map(|binding| {
            let (rank, label) = binding.hint?;
            let mut keys = display(binding.keys.first()?);
            if let Some(other) = binding.pair.and_then(first_key) {
                keys = format!("{keys}/{other}");
            }
            Some(Hint {
                action: binding.action,
                keys,
                label,
                rank,
                available: available(binding.action),
            })
        })
        .collect();
    shown.sort_by_key(|hint| hint.rank);
    shown
}

/// The hints that fit in `budget` when each takes `width` and `gap` separates them: the
/// highest ranks make way first, except the cheat sheet's, which always stays. Returns their
/// indices in rank order.
#[must_use]
pub fn fit(hints: &[Hint], budget: f32, width: impl Fn(&Hint) -> f32, gap: f32) -> Vec<usize> {
    let mut kept: Vec<usize> = (0..hints.len()).collect();
    let total = |kept: &[usize]| -> f32 {
        let widths: f32 = kept
            .iter()
            .filter_map(|&index| hints.get(index))
            .map(&width)
            .sum();
        widths + gap * crate::scroll::float(kept.len().saturating_sub(1))
    };
    while total(&kept) > budget {
        // The latest of the highest ranks, never the cheat sheet's.
        let Some(position) = kept
            .iter()
            .enumerate()
            .filter(|&(_, &index)| hints.get(index).is_some_and(|hint| hint.rank != u8::MAX))
            .max_by_key(|&(position, &index)| {
                (hints.get(index).map_or(0, |hint| hint.rank), position)
            })
            .map(|(position, _)| position)
        else {
            break;
        };
        kept.remove(position);
    }
    kept
}

/// How a keystroke sequence reads in the cheat sheet: `g g`, `Ctrl+D`, `Shift+G`.
#[must_use]
pub fn display(keys: &str) -> String {
    let strokes: Vec<String> = keys
        .split_whitespace()
        .map(|stroke| {
            let parts: Vec<&str> = stroke.split('-').collect();
            // A trailing `-` is the minus key itself, as in `ctrl--`.
            let (modifiers, key): (&[&str], &str) = match parts.as_slice() {
                [modifiers @ .., "", ""] => (modifiers, "-"),
                [modifiers @ .., key] => (modifiers, key),
                [] => (&[], ""),
            };
            let key = match key {
                "space" => "Space".to_owned(),
                "enter" => "⏎".to_owned(),
                "escape" => "Esc".to_owned(),
                "tab" => "Tab".to_owned(),
                "up" => "↑".to_owned(),
                "down" => "↓".to_owned(),
                "left" => "←".to_owned(),
                "right" => "→".to_owned(),
                "pagedown" => "PgDn".to_owned(),
                "pageup" => "PgUp".to_owned(),
                "home" => "Home".to_owned(),
                "end" => "End".to_owned(),
                "insert" => "Ins".to_owned(),
                other if other.len() > 1 => other.to_uppercase(),
                other => other.to_owned(),
            };
            // A shifted letter reads as the capital it types; with Ctrl or Alt, letters are
            // capitals anyway, as menus write them.
            let letter = key.len() == 1 && key.chars().all(|c| c.is_ascii_alphabetic());
            let shifted_letter = letter && modifiers == ["shift"];
            let key = if letter && !modifiers.is_empty() {
                key.to_uppercase()
            } else {
                key
            };
            if shifted_letter {
                return key;
            }
            let mut shown: Vec<String> = modifiers
                .iter()
                .map(|modifier| match *modifier {
                    "ctrl" => "Ctrl".to_owned(),
                    "shift" => "Shift".to_owned(),
                    "alt" => "Alt".to_owned(),
                    other => other.to_owned(),
                })
                .collect();
            shown.push(key);
            shown.join("+")
        })
        .collect::<Vec<_>>();
    // A sequence of plain characters reads as vim writes it: `gg`, `za`.
    let separator = if strokes.iter().all(|stroke| stroke.chars().count() == 1) {
        ""
    } else {
        " "
    };
    strokes.join(separator)
}

/// One cheat-sheet section: its title and its (keys, help) entries.
pub type Section = (&'static str, Vec<(String, &'static str)>);

/// Deals `sections` into `count` columns, each section going to the shortest column so far.
#[must_use]
pub fn columns(sections: Vec<Section>, count: usize) -> Vec<Vec<Section>> {
    let mut columns: Vec<(usize, Vec<Section>)> = (0..count).map(|_| (0, Vec::new())).collect();
    for section in sections {
        if let Some((height, column)) = columns.iter_mut().min_by_key(|(height, _)| *height) {
            // A title takes about as much room as three entries.
            *height += section.1.len() + 3;
            column.push(section);
        }
    }
    columns.into_iter().map(|(_, column)| column).collect()
}

/// The cheat sheet's sections: each group's entries as (keys, help), in keymap order.
#[must_use]
pub fn sheet(bindings: &[Binding]) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for binding in bindings {
        if binding.help.is_empty() || binding.keys.is_empty() {
            continue;
        }
        // The first two keys; the rest are alternatives such as arrow keys.
        let keys = binding
            .keys
            .iter()
            .take(2)
            .map(|key| display(key))
            .collect::<Vec<_>>()
            .join("  ");
        match sections
            .iter_mut()
            .find(|(group, _)| *group == binding.group)
        {
            Some((_, entries)) => entries.push((keys, binding.help)),
            None => sections.push((binding.group, vec![(keys, binding.help)])),
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An override replaces an action's keys everywhere it is bound, an empty list unbinds it,
    /// and unknown actions and bad keystrokes are reported.
    #[test]
    fn overrides_replace_unbind_and_report() {
        let overrides = BTreeMap::from([
            ("NextHunk".to_owned(), vec!["ctrl-j".to_owned()]),
            ("ToggleOverview".to_owned(), Vec::new()),
            ("Nonsense".to_owned(), vec!["x".to_owned()]),
            ("ToggleFlag".to_owned(), vec!["bogus-x".to_owned()]),
        ]);
        let (bindings, problems) = keymap(&overrides);
        let keys = |action: &str| -> Vec<String> {
            bindings
                .iter()
                .filter(|binding| binding.action == action)
                .flat_map(|binding| binding.keys.clone())
                .collect()
        };
        assert_eq!(keys("NextHunk"), ["ctrl-j"]);
        assert_eq!(keys("ToggleOverview"), Vec::<String>::new());
        assert_eq!(keys("PreviousHunk"), ["k", "up"]);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    /// The bar shows a mode's ranked actions, pairs keys, follows overrides and leaves out
    /// unavailable actions.
    #[test]
    fn hints_follow_rank_pairs_and_availability() {
        let overrides = BTreeMap::from([("ToggleApprove".to_owned(), vec!["a".to_owned()])]);
        let (bindings, _) = keymap(&overrides);
        let shown = hints(&bindings, Area::Diff, |action| action != "NextFile");
        let first: Vec<(&str, &str)> = shown
            .iter()
            .take(3)
            .map(|hint| (hint.keys.as_str(), hint.label))
            .collect();
        assert_eq!(first, [("j/k", "hunk"), ("a", "approve"), ("/", "find")]);
        assert_eq!(
            shown.last().map(|hint| (hint.keys.as_str(), hint.label)),
            Some(("?", "keys")),
            "the keys sheet comes last"
        );
        assert!(
            shown
                .iter()
                .any(|hint| hint.label == "next file" && !hint.available),
            "an unavailable action keeps its place, dimmed"
        );
        for label in ["files", "grep", "style", "split", "settings"] {
            assert!(
                shown.iter().any(|hint| hint.label == label),
                "{label} is hinted"
            );
        }
    }

    /// A narrow bar drops the highest ranks first, keeps the cheat sheet's hint last, and
    /// never splits a hint.
    #[test]
    fn hints_fit_by_rank() {
        let hint = |rank: u8, label: &'static str| Hint {
            action: label,
            keys: "x".to_owned(),
            label,
            rank,
            available: true,
        };
        let hints = [
            hint(1, "a"),
            hint(2, "b"),
            hint(9, "c"),
            hint(u8::MAX, "keys"),
        ];
        let width = |_: &Hint| 10.0;
        assert_eq!(fit(&hints, 100.0, width, 2.0), [0, 1, 2, 3]);
        assert_eq!(fit(&hints, 35.0, width, 2.0), [0, 1, 3]);
        assert_eq!(fit(&hints, 22.0, width, 2.0), [0, 3]);
        assert_eq!(
            fit(&hints, 5.0, width, 2.0),
            [3],
            "the cheat sheet's hint always stays"
        );
    }

    /// Each area's key context names it alone.
    #[test]
    fn key_contexts_name_their_areas() {
        for &area in Area::ALL {
            assert_eq!(Area::from_context(area.context()), Some(area));
        }
        assert_eq!(Area::from_context("Input"), None);
    }

    /// Every default keystroke parses, and every default action exists.
    #[test]
    fn defaults_are_valid() {
        let (_, problems) = keymap(&BTreeMap::new());
        assert_eq!(problems, Vec::<String>::new());
    }

    #[test]
    fn keys_display_like_a_cheat_sheet() {
        assert_eq!(display("g g"), "gg");
        assert_eq!(display("ctrl-d"), "Ctrl+D");
        assert_eq!(display("shift-g"), "G");
        assert_eq!(display("ctrl--"), "Ctrl+-");
        assert_eq!(display("z shift-m"), "zM");
        assert_eq!(display("g ctrl-d"), "g Ctrl+D");
        assert_eq!(display("ctrl-shift-e"), "Ctrl+Shift+E");
        assert_eq!(display("f12"), "F12");
    }
}
