//! The files pane's width: the limits its divider stays within, so the diff is never squeezed
//! away.

/// The narrowest sidebar.
pub const SIDEBAR_MIN: f32 = 180.0;
/// The widest sidebar.
pub const SIDEBAR_MAX: f32 = 560.0;
/// The narrowest the changes may get while the divider is dragged.
pub const CHANGES_MIN: f32 = 400.0;

/// The sidebar width to show for a `wanted` width in a `window` that wide.
#[must_use]
pub fn sidebar_width(wanted: f32, window: f32) -> f32 {
    let max = SIDEBAR_MAX.min(window - CHANGES_MIN).max(SIDEBAR_MIN);
    wanted.clamp(SIDEBAR_MIN, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Widths stay within their limits and always leave room for the changes.
    #[test]
    fn widths_stay_within_limits() {
        assert_eq!(sidebar_width(50.0, 1600.0), SIDEBAR_MIN);
        assert_eq!(sidebar_width(900.0, 1600.0), SIDEBAR_MAX);
        assert_eq!(sidebar_width(500.0, 800.0), 400.0);
    }
}
