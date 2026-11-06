//! The files pane's width: the limits its divider stays within, so the diff is never squeezed
//! away, in a window's logical pixels and in a terminal's cells.

/// The narrowest sidebar.
pub const SIDEBAR_MIN: f32 = 180.0;
/// The narrowest sidebar in cells of its text, so a large font still leaves room for names.
const SIDEBAR_MIN_CELLS: f32 = 16.0;
/// The widest sidebar.
pub const SIDEBAR_MAX: f32 = 560.0;
/// The narrowest the changes may get while the divider is dragged.
pub const CHANGES_MIN: f32 = 400.0;

/// The sidebar width to show for a `wanted` width in a `window` that wide, with text cells
/// `cell` pixels wide.
#[must_use]
pub fn sidebar_width(wanted: f32, window: f32, cell: f32) -> f32 {
    let min = SIDEBAR_MIN.max(cell * SIDEBAR_MIN_CELLS);
    let max = SIDEBAR_MAX.min(window - CHANGES_MIN).max(min);
    wanted.clamp(min, max)
}

/// The narrowest sidebar in a terminal, in cells.
pub const SIDEBAR_MIN_CELLS_TERMINAL: u16 = 16;
/// The widest sidebar in a terminal, in cells.
pub const SIDEBAR_MAX_CELLS: u16 = 62;
/// The narrowest the changes may get beside the sidebar in a terminal, in cells.
pub const CHANGES_MIN_CELLS: u16 = 44;

/// The sidebar width to show for a `wanted` width in a terminal `columns` cells wide, all in
/// cells: as in a window, the changes keep their room first, and a terminal too narrow for
/// both gives the sidebar half the width.
#[must_use]
pub fn sidebar_cells(wanted: f32, columns: u16) -> u16 {
    let max = SIDEBAR_MAX_CELLS
        .min(columns.saturating_sub(CHANGES_MIN_CELLS))
        .max(SIDEBAR_MIN_CELLS_TERMINAL)
        .min(columns / 2);
    let min = SIDEBAR_MIN_CELLS_TERMINAL.min(max);
    let wanted = if wanted.is_finite() {
        wanted.round().clamp(0.0, f32::from(u16::MAX))
    } else {
        0.0
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a width clamped to the range of u16 above"
    )]
    let wanted = wanted as u16;
    wanted.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Widths stay within their limits and always leave room for the changes.
    #[test]
    fn widths_stay_within_limits() {
        assert_eq!(sidebar_width(50.0, 1600.0, 9.0), SIDEBAR_MIN);
        assert_eq!(sidebar_width(900.0, 1600.0, 9.0), SIDEBAR_MAX);
        assert_eq!(sidebar_width(500.0, 800.0, 9.0), 400.0);
    }

    /// A terminal's sidebar keeps within its limits in cells, leaves the changes their room, and
    /// takes at most half of a narrow terminal.
    #[test]
    fn terminal_widths_stay_within_limits() {
        assert_eq!(sidebar_cells(32.0, 160), 32);
        assert_eq!(sidebar_cells(4.0, 160), SIDEBAR_MIN_CELLS_TERMINAL);
        assert_eq!(sidebar_cells(500.0, 200), SIDEBAR_MAX_CELLS);
        assert_eq!(sidebar_cells(50.0, 80), 36);
        assert_eq!(sidebar_cells(32.0, 24), 12);
        assert_eq!(sidebar_cells(f32::NAN, 160), SIDEBAR_MIN_CELLS_TERMINAL);
    }

    /// A large font widens the narrowest sidebar, so its names keep room.
    #[test]
    fn large_fonts_widen_the_narrowest_sidebar() {
        assert_eq!(sidebar_width(50.0, 1600.0, 20.0), 320.0);
        assert_eq!(sidebar_width(500.0, 1600.0, 20.0), 500.0);
    }
}
