//! The actions keys trigger, by the names the keymap table gives them, and what each does.
//!
//! The variant names are exactly the action names of [`crate::keymap`], so `[keys]` overrides,
//! QA mode's `action <name>` and the front ends' key bindings all name the same actions.

use review_core::MarkVerdict;

use super::picker::Kind;
use super::{Effect, Mode, Model, Overlay, QuitKind, Screen, SheetStep};

/// Declares [`Action`] with its variants' names.
macro_rules! actions {
    ($($(#[$doc:meta])* $name:ident,)*) => {
        /// An action a key triggers, named as the keymap names it.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Action {
            $($(#[$doc])* $name,)*
        }

        impl Action {
            /// Every action.
            pub const ALL: &[Self] = &[$(Self::$name,)*];

            /// The action's name, as the keymap and `[keys]` name it.
            #[must_use]
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$name => stringify!($name),)*
                }
            }
        }
    };
}

actions! {
    /// Moves the tree cursor down, previewing the file it lands on.
    TreeDown,
    /// Moves the tree cursor up, previewing the file it lands on.
    TreeUp,
    /// Folds the folder under the tree cursor, or moves to its parent.
    TreeCollapse,
    /// Unfolds the folder under the tree cursor, or opens the file's diff.
    TreeExpand,
    /// In the tree, folds or unfolds a folder or opens a file's diff; in the diff, follows the
    /// selected hunk's move or expands a collapsed unit.
    Activate,
    /// Opens the diff of the file under the tree cursor.
    FocusDiff,
    /// Folds every folder.
    CloseAllFolds,
    /// Unfolds every folder.
    OpenAllFolds,
    /// Selects the next hunk.
    NextHunk,
    /// Selects the previous hunk.
    PreviousHunk,
    /// Selects the first hunk.
    FirstHunk,
    /// Selects the last hunk.
    LastHunk,
    /// Scrolls the diff, or moves the buffer cursor, half a page down.
    HalfPageDown,
    /// Scrolls the diff, or moves the buffer cursor, half a page up.
    HalfPageUp,
    /// Pans the code of the diff or the buffer half its width right, to see the end of long
    /// lines.
    PanRight,
    /// Pans the code of the diff or the buffer half its width back left.
    PanLeft,
    /// Folds or unfolds the selected hunk.
    ToggleFold,
    /// Steps to the next match of a diff search, or else opens the next file to review.
    NextFile,
    /// Goes back from the diff to the files.
    FocusTree,
    /// Opens the whole file at the selected hunk, or goes back to the diff.
    ToggleBuffer,
    /// Approves what the keys act on, or takes the approval back.
    ToggleApprove,
    /// Flags what the keys act on as needing review, or takes the flag back.
    ToggleFlag,
    /// Takes what the keys act on back to unreviewed.
    ResetReview,
    /// Switches to a newer revision, or fetches one.
    Refresh,
    /// Trusts the review after a confirming second press, or revokes trust.
    ToggleTrust,
    /// Copies the selected hunk's first unit ID.
    YankUnitId,
    /// Moves the buffer cursor down a line.
    CursorDown,
    /// Moves the buffer cursor up a line.
    CursorUp,
    /// Moves the buffer cursor to the next identifier.
    NextWord,
    /// Moves the buffer cursor to the previous identifier.
    PreviousWord,
    /// Moves the buffer cursor to the first line.
    BufferTop,
    /// Moves the buffer cursor to the last line.
    BufferBottom,
    /// Filters the tree, or finds in the diff or the buffer.
    OpenFind,
    /// Steps to the next match.
    FindNext,
    /// Steps to the previous match.
    FindPrevious,
    /// Goes to the definition of the identifier under the buffer cursor.
    GoToDefinition,
    /// Goes back to the buffer a definition jump left.
    JumpBack,
    /// In the buffer, closes the find results, then returns from a definition, then goes back
    /// to the diff.
    Dismiss,
    /// Cancels the search or the tree's filter being typed.
    CloseFind,
    /// Opens the pull request overview.
    ToggleOverview,
    /// Applies the next diff style.
    ToggleDiffStyle,
    /// Widens the diff across the window.
    ScreenNext,
    /// Brings the files back beside the diff.
    ScreenPrevious,
    /// Shows or hides the files.
    ToggleSidebar,
    /// Opens the cheat sheet.
    ToggleHelp,
    /// Closes the open overlay.
    CloseOverlay,
    /// Scrolls the cheat sheet or the overview a line down.
    SheetDown,
    /// Scrolls the cheat sheet or the overview a line up.
    SheetUp,
    /// Scrolls the cheat sheet or the overview half its height down.
    SheetPageDown,
    /// Scrolls the cheat sheet or the overview half its height up.
    SheetPageUp,
    /// Copies the selected text.
    Copy,
    /// Clears the tree's filter.
    ClearFilter,
    /// Opens the file finder.
    FindFiles,
    /// Opens the content search.
    SearchContent,
    /// Selects the picker's next entry.
    PickerNext,
    /// Selects the picker's previous entry.
    PickerPrevious,
    /// Opens the picker's selected entry.
    PickerOpen,
    /// Switches the picker between the changed files and the whole tree.
    PickerScope,
    /// Gives the picker's list the keys.
    PickerBrowse,
    /// Gives the picker's query the keys.
    PickerEdit,
    /// Closes the picker.
    PickerClose,
    /// Makes the code larger.
    ZoomIn,
    /// Makes the code smaller.
    ZoomOut,
    /// Puts the code back at its configured size.
    ZoomReset,
    /// Toggles full screen.
    ToggleFullscreen,
    /// Closes the window, which quits.
    CloseWindow,
    /// Quits.
    Quit,
    /// Opens the settings list.
    OpenSettings,
    /// Shows the diff side by side or unified.
    ToggleLayout,
    /// Shows approved hunks or folds them.
    ToggleApproved,
    /// Moves the settings cursor down, previewing the entry.
    SettingsNext,
    /// Moves the settings cursor up, previewing the entry.
    SettingsPrevious,
    /// Applies and remembers the settings entry under the cursor.
    SettingsApply,
    /// Gives the settings search field the keys.
    SettingsSearch,
    /// Gives the settings list the keys, keeping the search.
    SettingsBrowse,
    /// Closes the settings list, keeping what was applied.
    SettingsClose,
}

impl Action {
    /// The action named `name`, as the keymap names it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|action| action.name() == name)
    }
}

impl Model {
    /// Does what `action` does where the keyboard is now; an action the front end cannot honor
    /// (see [`crate::front_end::Capabilities::allows`]) does nothing.
    pub fn dispatch(&mut self, action: Action) {
        if !self.settings.front_end.capabilities().allows(action) {
            return;
        }
        match action {
            Action::TreeDown => self.tree_move(1),
            Action::TreeUp => self.tree_move(-1),
            Action::TreeCollapse => self.tree_collapse(),
            Action::TreeExpand => self.tree_expand(),
            Action::Activate => match self.mode {
                Mode::Tree => self.tree_activate(),
                Mode::Diff => self.diff_activate(),
                Mode::Buffer => {}
            },
            Action::FocusDiff => self.open_diff(),
            Action::CloseAllFolds => self.tree_fold_all(true),
            Action::OpenAllFolds => self.tree_fold_all(false),
            Action::NextHunk => self.select_hunk(self.hunk + 1),
            Action::PreviousHunk => self.select_hunk(self.hunk.saturating_sub(1)),
            Action::FirstHunk => self.select_hunk(0),
            Action::LastHunk => self.select_hunk(usize::MAX),
            Action::HalfPageDown => self.half_page(true),
            Action::HalfPageUp => self.half_page(false),
            Action::PanRight => self.pan(true),
            Action::PanLeft => self.pan(false),
            Action::ToggleFold => self.toggle_fold(),
            Action::NextFile => self.next_file(),
            Action::FocusTree => self.focus_tree(),
            Action::ToggleBuffer => self.toggle_buffer(),
            Action::ToggleApprove => self.review(MarkVerdict::Accepted),
            Action::ToggleFlag => self.review(MarkVerdict::Flagged),
            Action::ResetReview => self.review(MarkVerdict::Unseen),
            Action::Refresh => self.refresh(),
            Action::ToggleTrust => self.toggle_trust(),
            Action::YankUnitId => self.yank_unit_id(),
            Action::CursorDown => self.move_buffer_cursor_by(1),
            Action::CursorUp => self.move_buffer_cursor_by(-1),
            Action::NextWord => self.move_word(true),
            Action::PreviousWord => self.move_word(false),
            Action::BufferTop => self.set_buffer_cursor(0, 0),
            Action::BufferBottom => self.set_buffer_cursor(usize::MAX, 0),
            Action::OpenFind => self.open_find(),
            Action::FindNext => self.find_step(true, false),
            Action::FindPrevious => self.find_step(false, false),
            Action::GoToDefinition => self.go_to_definition(),
            Action::JumpBack => self.jump_back(),
            Action::Dismiss => self.buffer_dismiss(),
            Action::CloseFind => self.close_find(),
            Action::ToggleOverview => self.open_overlay(Overlay::Overview),
            Action::ToggleDiffStyle => self.next_diff_style(),
            Action::ScreenNext => self.screen = Screen::Full,
            Action::ScreenPrevious => self.screen_previous(),
            Action::ToggleSidebar => self.toggle_sidebar(),
            Action::ToggleHelp => self.open_overlay(Overlay::Help),
            Action::CloseOverlay => self.close_overlay(),
            Action::SheetDown => self.effect(Effect::ScrollSheet(SheetStep::Lines(1))),
            Action::SheetUp => self.effect(Effect::ScrollSheet(SheetStep::Lines(-1))),
            Action::SheetPageDown => self.effect(Effect::ScrollSheet(SheetStep::HalfPages(1))),
            Action::SheetPageUp => self.effect(Effect::ScrollSheet(SheetStep::HalfPages(-1))),
            Action::Copy => self.copy(),
            Action::ClearFilter => self.clear_filter(),
            Action::FindFiles => self.open_picker(Kind::Files),
            Action::SearchContent => self.open_picker(Kind::Content),
            Action::PickerNext => self.picker_step(true),
            Action::PickerPrevious => self.picker_step(false),
            Action::PickerOpen => self.picker_open(),
            Action::PickerScope => self.picker_scope(),
            Action::PickerBrowse => self.picker_typing(false),
            Action::PickerEdit => self.picker_typing(true),
            Action::PickerClose => self.close_picker(),
            Action::ZoomIn => self.set_code_size(self.code_size + 1.0),
            Action::ZoomOut => self.set_code_size(self.code_size - 1.0),
            Action::ZoomReset => self.set_code_size(self.default_code_size),
            Action::ToggleFullscreen => self.effect(Effect::Fullscreen),
            Action::CloseWindow | Action::Quit => self.quit(QuitKind::Interactive),
            Action::OpenSettings => self.open_settings(),
            Action::ToggleLayout => self.toggle_layout(),
            Action::ToggleApproved => self.apply_approved(self.folds.approved.toggled()),
            Action::SettingsNext => self.settings_step(true),
            Action::SettingsPrevious => self.settings_step(false),
            Action::SettingsApply => self.apply_cursor(),
            Action::SettingsSearch => self.settings_typing(true),
            Action::SettingsBrowse => self.settings_typing(false),
            Action::SettingsClose => self.close_settings(),
        }
    }
}
