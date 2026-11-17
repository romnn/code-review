//! The files pane's width: as wide as its tree needs unless the reviewer dragged its divider,
//! within limits that never squeeze the diff away, in a window's logical pixels and in a
//! terminal's cells.

/// How wide the files pane is, as the reviewer left it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SidebarWidth {
    /// As wide as the tree's widest row needs, every folder open, at least [`FIT_MIN_CELLS`].
    Fit,
    /// The width the reviewer dragged the divider to, in the front end's units: logical pixels
    /// in review-gui and cells in review-tui.
    Set(f32),
}

/// The widest sidebar's share of the window or terminal, so the changes keep most of it.
const MAX_SHARE: f32 = 0.25;

/// The narrowest a sidebar fitted to its tree gets, in cells of its text, so a tree of short
/// names still leaves its progress lines their bars.
pub const FIT_MIN_CELLS: u16 = 24;

/// The narrowest sidebar.
pub const SIDEBAR_MIN: f32 = 180.0;
/// The narrowest sidebar in cells of its text, so a large font still leaves room for names.
const SIDEBAR_MIN_CELLS: f32 = 16.0;
/// The narrowest the changes may get beside the sidebar.
pub const CHANGES_MIN: f32 = 400.0;

/// The sidebar width to show in a `window` that wide, with text cells `cell` pixels wide:
/// `width` when the reviewer set one, or else `fit`, what the tree's widest row takes, at least
/// [`FIT_MIN_CELLS`].
///
/// Either way the sidebar takes at most a quarter of the window, and within that it is at least
/// [`SIDEBAR_MIN`] and 16 cells wide, and otherwise leaves the changes [`CHANGES_MIN`].
#[must_use]
pub fn sidebar_width(width: SidebarWidth, fit: f32, window: f32, cell: f32) -> f32 {
    let narrowest = SIDEBAR_MIN.max(cell * SIDEBAR_MIN_CELLS);
    let max = (window * MAX_SHARE)
        .min((window - CHANGES_MIN).max(narrowest))
        .max(0.0);
    let min = narrowest.min(max);
    let wanted = match width {
        SidebarWidth::Set(width) => width,
        SidebarWidth::Fit => fit.max(cell * f32::from(FIT_MIN_CELLS)),
    };
    if wanted.is_nan() {
        min
    } else {
        wanted.clamp(min, max)
    }
}

/// The narrowest sidebar in a terminal, in cells.
pub const SIDEBAR_MIN_CELLS_TERMINAL: u16 = 16;
/// The narrowest the changes may get beside the sidebar in a terminal, in cells.
pub const CHANGES_MIN_CELLS: u16 = 44;

/// The sidebar width to show in a terminal `columns` cells wide, all in cells: `width` when
/// the reviewer set one, or else `fit`, what the tree's widest row takes, at least
/// [`FIT_MIN_CELLS`].
///
/// As in a window, the sidebar takes at most a quarter, and within that it is at least
/// [`SIDEBAR_MIN_CELLS_TERMINAL`] wide and otherwise leaves the changes [`CHANGES_MIN_CELLS`].
#[must_use]
pub fn sidebar_cells(width: SidebarWidth, fit: u16, columns: u16) -> u16 {
    let share = columns / 4;
    let max = share.min(
        columns
            .saturating_sub(CHANGES_MIN_CELLS)
            .max(SIDEBAR_MIN_CELLS_TERMINAL),
    );
    let min = SIDEBAR_MIN_CELLS_TERMINAL.min(max);
    let wanted = match width {
        SidebarWidth::Set(width) => whole_cells(width),
        SidebarWidth::Fit => fit.max(FIT_MIN_CELLS),
    };
    wanted.clamp(min, max)
}

/// `width` in whole cells, zero when it is no number.
fn whole_cells(width: f32) -> u16 {
    let width = if width.is_finite() {
        width.round().clamp(0.0, f32::from(u16::MAX))
    } else {
        0.0
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a width clamped to the range of u16 above"
    )]
    let width = width as u16;
    width
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A width the reviewer set stays within the limits: at most a quarter of the window, and never
    /// so wide that the changes get less than their least.
    #[test]
    fn widths_stay_within_limits() {
        let set = SidebarWidth::Set;
        assert_eq!(sidebar_width(set(50.0), 0.0, 1600.0, 9.0), SIDEBAR_MIN);
        assert_eq!(sidebar_width(set(900.0), 0.0, 1600.0, 9.0), 400.0);
        assert_eq!(sidebar_width(set(500.0), 0.0, 1200.0, 9.0), 300.0);
        assert_eq!(sidebar_width(set(500.0), 0.0, 640.0, 9.0), 160.0);
        // A window too small for the narrowest sidebar beside the changes' least keeps a
        // quarter.
        assert_eq!(sidebar_width(set(500.0), 0.0, 500.0, 9.0), 125.0);
        assert_eq!(sidebar_width(set(500.0), 0.0, 300.0, 9.0), 75.0);
        assert_eq!(sidebar_width(set(f32::NAN), 0.0, 1600.0, 9.0), SIDEBAR_MIN);
    }

    /// A fitted sidebar is as wide as its tree needs, at least 24 cells and at most a quarter of
    /// the window.
    #[test]
    fn a_fitted_width_follows_the_tree_within_limits() {
        let fit = SidebarWidth::Fit;
        assert_eq!(sidebar_width(fit, 350.0, 1600.0, 9.0), 350.0);
        assert_eq!(sidebar_width(fit, 100.0, 1600.0, 9.0), 216.0);
        assert_eq!(sidebar_width(fit, 900.0, 1600.0, 9.0), 400.0);
        assert_eq!(sidebar_width(fit, 900.0, 4000.0, 9.0), 900.0);
    }

    /// Where the floor of a fitted sidebar or the narrowest sidebar would pass a quarter of the
    /// window, as in a small window with a large font, the quarter wins.
    #[test]
    fn floors_give_way_to_the_widest() {
        let fit = SidebarWidth::Fit;
        // 24 cells of 20 px are 480 px, past a quarter of 1000 px.
        assert_eq!(sidebar_width(fit, 0.0, 1000.0, 20.0), 250.0);
        // 16 cells of 20.4 px are 326.4 px, past a quarter of 640 px.
        assert_eq!(sidebar_width(fit, 0.0, 640.0, 20.4), 160.0);
        assert_eq!(
            sidebar_width(SidebarWidth::Set(500.0), 0.0, 640.0, 20.4),
            160.0
        );
    }

    /// A terminal's sidebar keeps within its limits in cells, leaves the changes their room,
    /// and takes at most a quarter even of a narrow terminal.
    #[test]
    fn terminal_widths_stay_within_limits() {
        let set = SidebarWidth::Set;
        assert_eq!(sidebar_cells(set(32.0), 0, 160), 32);
        assert_eq!(sidebar_cells(set(4.0), 0, 160), SIDEBAR_MIN_CELLS_TERMINAL);
        assert_eq!(sidebar_cells(set(500.0), 0, 200), 50);
        assert_eq!(sidebar_cells(set(50.0), 0, 80), 20);
        assert_eq!(sidebar_cells(set(32.0), 0, 50), 12);
        assert_eq!(sidebar_cells(set(32.0), 0, 24), 6);
        assert_eq!(
            sidebar_cells(set(f32::NAN), 0, 160),
            SIDEBAR_MIN_CELLS_TERMINAL
        );
    }

    /// A fitted terminal sidebar follows its tree between 24 cells and a quarter, and in a terminal
    /// too narrow for the floor takes what the limits allow.
    #[test]
    fn a_fitted_terminal_width_follows_the_tree_within_limits() {
        let fit = SidebarWidth::Fit;
        assert_eq!(sidebar_cells(fit, 30, 160), 30);
        assert_eq!(sidebar_cells(fit, 10, 160), FIT_MIN_CELLS);
        assert_eq!(sidebar_cells(fit, 90, 160), 40);
        // A quarter of 50 columns is less than the narrowest sidebar, and wins.
        assert_eq!(sidebar_cells(fit, 30, 50), 12);
        assert_eq!(sidebar_cells(fit, 30, 24), 6);
    }

    /// A large font widens the narrowest sidebar, so its names keep room.
    #[test]
    fn large_fonts_widen_the_narrowest_sidebar() {
        let set = SidebarWidth::Set;
        assert_eq!(sidebar_width(set(50.0), 0.0, 1600.0, 20.0), 320.0);
        assert_eq!(sidebar_width(set(500.0), 0.0, 1600.0, 20.0), 400.0);
    }
}
