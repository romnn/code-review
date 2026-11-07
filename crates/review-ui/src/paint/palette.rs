//! The window's colors by role, from the theme and the diff style.

use review_core::FileTone;

use crate::color::{Hsla, rgb};
use crate::config::Background;
use crate::look;
use crate::model::Model;
use crate::rows::Badge;
use crate::state::Progress;
use crate::theme::{self, Theme};

/// The colors of the window, by role, from the theme and the configured diff colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// The editor's background, behind code.
    pub background: Hsla,
    /// Panes' and bars' background.
    pub surface: Hsla,
    /// Behind the rows of a hunk and the tool's own rows.
    pub band: Hsla,
    /// A step above the band: hunk headers, the cursor's row, chips.
    pub raised: Hsla,
    /// Rules and borders.
    pub hairline: Hsla,
    /// Text that matters.
    pub text: Hsla,
    /// Quieter text.
    pub dim: Hsla,
    /// The quietest text: line numbers, details, hints.
    pub faint: Hsla,
    /// The selected hunk, focus and keys.
    pub accent: Hsla,
    /// A text selection, translucent.
    pub selection: Hsla,
    /// A find match, translucent.
    pub find: Hsla,
    /// Failures.
    pub error: Hsla,
    /// Review state: nothing reviewed yet.
    pub open: Hsla,
    /// Review state: approved.
    pub accepted: Hsla,
    /// Review state: flagged.
    pub flagged: Hsla,
    /// Review state: changed since reviewed.
    pub changed: Hsla,
    /// Moved code.
    pub moved: Hsla,
    /// Collapsed and merged things, such as a merged pull request.
    pub collapsed: Hsla,
    /// Change kind: added file.
    pub kind_added: Hsla,
    /// Change kind: modified file.
    pub kind_modified: Hsla,
    /// Change kind: deleted file.
    pub kind_deleted: Hsla,
    /// Change kind: renamed file.
    pub kind_renamed: Hsla,
    /// Change kind: type change.
    pub kind_type: Hsla,
    /// Change kind: formatting-only or generated, and the tags of quiet hunks.
    pub kind_quiet: Hsla,
    /// A removed line's background, after the diff style's muting.
    pub removed_line: Hsla,
    /// An added line's background, likewise.
    pub added_line: Hsla,
    /// A formatting-only line's background.
    pub quiet_line: Hsla,
    /// A removed line's text, in a style without syntax colors.
    pub removed_text: Hsla,
    /// An added line's text, likewise.
    pub added_text: Hsla,
    /// The `−` sign and the removed line count.
    pub removed_sign: Hsla,
    /// The `+` sign and the added line count.
    pub added_sign: Hsla,
}

impl Palette {
    /// The palette of the look on screen.
    #[must_use]
    pub fn of(model: &Model) -> Self {
        Self::new(
            model.theme(),
            model.settings.removed,
            model.settings.added,
            model.look().style.background,
        )
    }

    /// The palette of `theme`, with the configured line backgrounds when there are any.
    ///
    /// Muted line backgrounds are mixed toward the editor background here.
    /// Everything that tints a line reads these final backgrounds, so lifting syntax colors to
    /// stay legible measures against them.
    #[must_use]
    pub fn new(
        theme: &Theme,
        removed: Option<u32>,
        added: Option<u32>,
        background: Background,
    ) -> Self {
        let line = |hex: Option<u32>, fallback: Hsla| {
            let color = hex.map_or(fallback, |hex| rgb(hex).into());
            match background {
                Background::None | Background::Bold => color,
                Background::Muted => theme::mix(color, theme.ui.background, look::MUTED_SHARE),
            }
        };
        Self {
            background: theme.ui.background,
            surface: theme.ui.panel,
            band: theme.ui.band,
            raised: theme.ui.raised,
            hairline: theme.ui.hairline,
            text: theme.ui.text,
            dim: theme.ui.dim,
            faint: theme.ui.faint,
            accent: theme.ui.accent,
            selection: theme.ui.selection.alpha(0.6),
            find: theme.ui.find.alpha(0.45),
            error: theme.ui.error,
            open: theme.review.open,
            accepted: theme.review.done,
            flagged: theme.review.flagged,
            changed: theme.review.changed,
            moved: theme.kind.moved,
            collapsed: theme.kind.quiet,
            kind_added: theme.kind.added,
            kind_modified: theme.kind.modified,
            kind_deleted: theme.kind.deleted,
            kind_renamed: theme.kind.renamed,
            kind_type: theme.kind.type_change,
            kind_quiet: theme.kind.quiet,
            removed_line: line(removed, theme.diff.removed_line),
            added_line: line(added, theme.diff.added_line),
            quiet_line: theme.diff.quiet_line,
            removed_text: theme.diff.removed_text,
            added_text: theme.diff.added_text,
            removed_sign: theme.diff.removed_sign,
            added_sign: theme.diff.added_sign,
        }
    }

    /// The color of a file's change tone.
    #[must_use]
    pub fn tone(&self, tone: FileTone) -> Hsla {
        match tone {
            FileTone::Added => self.kind_added,
            FileTone::Modified => self.kind_modified,
            FileTone::Deleted => self.kind_deleted,
            FileTone::Renamed => self.kind_renamed,
            FileTone::Moved => self.moved,
            FileTone::TypeChanged => self.kind_type,
            FileTone::FormattingOnly | FileTone::Generated => self.kind_quiet,
        }
    }

    /// The color of a review progress.
    #[must_use]
    pub fn progress(&self, progress: Progress) -> Hsla {
        match progress {
            Progress::Open | Progress::Partial => self.open,
            Progress::Done => self.accepted,
            Progress::Flagged => self.flagged,
            Progress::Changed => self.changed,
        }
    }

    /// The color of a hunk badge.
    #[must_use]
    pub fn badge(&self, badge: Badge) -> Hsla {
        match badge {
            Badge::Unreviewed => self.open,
            Badge::Seen | Badge::Accepted => self.accepted,
            // Approval without a mark reads quieter than the reviewer's own.
            Badge::Auto => self.accepted.alpha(0.6),
            Badge::Flagged => self.flagged,
            Badge::Changed => self.changed,
        }
    }

    /// The colors of a selected hunk header's text on the accent's solid blue: one ink that
    /// reads on it, with quieter parts softened rather than tinted.
    #[must_use]
    pub fn on_accent(&self) -> Self {
        let ink = if self.accent.l > 0.5 {
            self.background
        } else {
            self.text
        };
        let soft = ink.alpha(0.75);
        Self {
            text: ink,
            dim: ink,
            faint: soft,
            kind_quiet: soft,
            open: ink,
            accepted: ink,
            flagged: ink,
            changed: ink,
            ..*self
        }
    }
}
