//! The files pane's width: the limits its divider stays within, so the diff is never squeezed
//! away.

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

    /// A large font widens the narrowest sidebar, so its names keep room.
    #[test]
    fn large_fonts_widen_the_narrowest_sidebar() {
        assert_eq!(sidebar_width(50.0, 1600.0, 20.0), 320.0);
        assert_eq!(sidebar_width(500.0, 1600.0, 20.0), 500.0);
    }
}
