//! The front ends that draw the model: what each can do, so the model offers only what it can
//! honor, and the settings each keeps apart from the other in the shared configuration file:
//! the files pane's width, and the mouse, which only review-tui's configuration narrows.
//!
//! Everything else is shared: the keys, the configuration, the themes, and the view state saved
//! with a review, such as the folded folders, so folds made in one front end show in the other.

use crate::config::{Config, LayoutConfig, Mouse};
use crate::model::Action;
use crate::panes::SidebarWidth;

/// A front end of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontEnd {
    /// review-gui: a window, measured in logical pixels.
    Window,
    /// review-tui: a terminal, measured in cells.
    Terminal,
}

impl FrontEnd {
    /// What the front end can do.
    #[must_use]
    pub fn capabilities(self) -> Capabilities {
        match self {
            Self::Window => Capabilities {
                code_size: true,
                window: true,
                smooth_scroll: true,
            },
            Self::Terminal => Capabilities {
                code_size: false,
                window: false,
                smooth_scroll: false,
            },
        }
    }

    /// The `[layout]` key that remembers the files pane's width in the front end's units.
    #[must_use]
    pub fn sidebar_key(self) -> &'static str {
        match self {
            Self::Window => "sidebar",
            Self::Terminal => "tui_sidebar",
        }
    }

    /// What the mouse may do in the front end: in a window everything, whatever `config` says,
    /// and in a terminal what its `[tui.mouse]` allows.
    ///
    /// # Errors
    ///
    /// In a terminal, says what is wrong with `[tui]`; a window never reads it.
    pub fn mouse(self, config: &Config) -> Result<Mouse, String> {
        match self {
            Self::Window => Ok(Mouse::default()),
            Self::Terminal => config.tui_mouse.clone(),
        }
    }

    /// The files pane's width `layout` remembers for the front end.
    #[must_use]
    pub fn sidebar_width(self, layout: &LayoutConfig) -> SidebarWidth {
        match self {
            Self::Window => layout.sidebar,
            Self::Terminal => layout.tui_sidebar,
        }
    }
}

/// What a front end can do; the model leaves out what it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Whether the code size can change: zoom, and the settings list's font sizes.
    pub code_size: bool,
    /// Whether the front end runs in a window of its own, which can go full screen and close
    /// apart from quitting; otherwise it runs in a terminal.
    pub window: bool,
    /// Whether scrolls the keys start ease to their target over a few frames rather than jump.
    pub smooth_scroll: bool,
}

impl Capabilities {
    /// Whether the front end can honor `action`; one it cannot is left out of the keymap and
    /// does nothing when dispatched.
    #[must_use]
    pub fn allows(self, action: Action) -> bool {
        match action {
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => self.code_size,
            Action::ToggleFullscreen | Action::CloseWindow => self.window,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window can do everything; a terminal has no code size and no window of its own.
    #[test]
    fn terminals_leave_out_what_they_cannot_do() {
        let window = FrontEnd::Window.capabilities();
        assert!(Action::ALL.iter().all(|&action| window.allows(action)));
        let terminal = FrontEnd::Terminal.capabilities();
        for action in [
            Action::ZoomIn,
            Action::ZoomOut,
            Action::ZoomReset,
            Action::ToggleFullscreen,
            Action::CloseWindow,
        ] {
            assert!(!terminal.allows(action), "{action:?}");
        }
        assert!(terminal.allows(Action::Quit));
        assert!(terminal.allows(Action::ToggleApprove));
    }

    /// Each front end reads its own width, in its own units, and fits its tree when the
    /// configuration has only the other's.
    #[test]
    fn sidebar_widths_are_each_front_ends_own() {
        let layout = LayoutConfig {
            sidebar: SidebarWidth::Set(420.0),
            ..LayoutConfig::default()
        };
        assert_eq!(
            FrontEnd::Window.sidebar_width(&layout),
            SidebarWidth::Set(420.0)
        );
        assert_eq!(FrontEnd::Terminal.sidebar_width(&layout), SidebarWidth::Fit);
        let layout = LayoutConfig {
            tui_sidebar: SidebarWidth::Set(40.0),
            ..layout
        };
        assert_eq!(
            FrontEnd::Terminal.sidebar_width(&layout),
            SidebarWidth::Set(40.0)
        );
        assert_eq!(
            FrontEnd::Window.sidebar_width(&layout),
            SidebarWidth::Set(420.0)
        );
    }
}
