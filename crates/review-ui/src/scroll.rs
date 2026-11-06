//! Smooth scrolling's arithmetic, in rows: eased, retargetable scroll animations, and whether and
//! where rows are revealed.

use std::time::{Duration, Instant};

/// Converts a row count or index to a float for pixel arithmetic.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "row counts stay far below 2^24, where f32 is exact"
)]
pub fn float(count: usize) -> f32 {
    count as f32
}

/// Converts a row count or index to a float for row arithmetic.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "row counts stay far below 2^53, where f64 is exact"
)]
pub fn rows(count: usize) -> f64 {
    count as f64
}

/// A distance in rows rounded to `f32`, as the quotient of two `f32` pixel lengths rounds.
///
/// A window measures in `f32` pixels, so the rows it sees are such quotients: a viewport a hair
/// short of a whole number of rows counts as that whole number, and a place in a list keeps the
/// row and fraction that quotient gives.
/// Rounding the exact rows once gives that same `f32`.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    reason = "offsets and viewports in rows are far within f32's range; the rounding is the point"
)]
pub fn narrow(rows: f64) -> f32 {
    rows as f32
}

/// The whole number of rows in a non-negative distance, rounded down.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to zero first, and row counts fit in usize"
)]
pub fn whole(value: f32) -> usize {
    value.max(0.0).floor() as usize
}

/// The whole number of rows in a non-negative distance in rows, rounded down after [`narrow`].
#[must_use]
pub fn whole_rows(rows: f64) -> usize {
    whole(narrow(rows))
}

/// How long a programmatic scroll takes.
pub const DURATION: Duration = Duration::from_millis(150);

/// A cubic ease-out over `t` in `0..=1`: fast at first, settling gently.
#[must_use]
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// A scroll animation of one axis, in rows from the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animation {
    from: f64,
    to: f64,
    started: Instant,
}

impl Animation {
    /// Starts an animation from `from` to `to` at `now`.
    #[must_use]
    pub fn new(from: f64, to: f64, now: Instant) -> Self {
        Self {
            from,
            to,
            started: now,
        }
    }

    /// The offset at `now`.
    #[must_use]
    pub fn value_at(&self, now: Instant) -> f64 {
        let t = now.saturating_duration_since(self.started).as_secs_f32() / DURATION.as_secs_f32();
        self.from + (self.to - self.from) * f64::from(ease_out(t))
    }

    /// Whether the animation has arrived at `now`.
    #[must_use]
    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= DURATION
    }

    /// Where the animation is heading.
    #[must_use]
    pub fn target(&self) -> f64 {
        self.to
    }

    /// Sends the animation to `to` from wherever it is at `now`, so a new jump continues the
    /// motion instead of restarting it.
    pub fn retarget(&mut self, to: f64, now: Instant) {
        *self = Self::new(self.value_at(now), to, now);
    }
}

/// Whether any of `rows` lies in a viewport `viewport` rows tall scrolled `offset` rows down.
///
/// A viewport not laid out yet, `None` or without height, shows everything.
#[must_use]
pub fn shows(rows: &std::ops::Range<usize>, offset: f64, viewport: Option<f64>) -> bool {
    let Some(viewport) = viewport.filter(|&viewport| viewport > 0.0) else {
        return true;
    };
    !rows.is_empty() && self::rows(rows.start) < offset + viewport && self::rows(rows.end) > offset
}

/// The offset that brings rows `top..bottom` into a viewport `height` rows tall at offset
/// `current`, keeping `margin` rows around them where they fit, or `None` when they are already
/// comfortably visible.
#[must_use]
pub fn reveal(current: f64, height: f64, top: f64, bottom: f64, margin: f64) -> Option<f64> {
    let margin = margin.min(((height - (bottom - top)) / 2.0).max(0.0));
    if top - margin < current {
        Some((top - margin).max(0.0))
    } else if bottom + margin > current + height {
        Some((bottom + margin - height).max(0.0))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easing_starts_fast_and_settles_on_the_target() {
        assert!(ease_out(0.0).abs() < f32::EPSILON);
        assert!((ease_out(1.0) - 1.0).abs() < f32::EPSILON);
        assert!(ease_out(0.25) > 0.25);
        assert!((ease_out(2.0) - 1.0).abs() < f32::EPSILON);
    }

    /// Retargeting mid-flight continues from the current position, without a jump.
    #[test]
    fn retargeting_continues_from_the_current_offset() {
        let start = Instant::now();
        let mut animation = Animation::new(0.0, 10.0, start);
        let mid = start + DURATION / 2;
        let before = animation.value_at(mid);
        animation.retarget(30.0, mid);
        assert!((animation.value_at(mid) - before).abs() < 0.001);
        assert!(!animation.done(mid));
        assert!((animation.value_at(mid + DURATION) - 30.0).abs() < 0.001);
        assert!(animation.done(mid + DURATION));
    }

    /// A hunk counts as shown when any of its rows is in the viewport, and only then.
    #[test]
    fn rows_show_when_any_is_in_the_viewport() {
        assert!(!shows(&(10..12), 0.0, Some(10.0)));
        assert!(shows(&(9..12), 0.0, Some(10.0)));
        assert!(shows(&(0..3), 2.5, Some(10.0)));
        assert!(!shows(&(0..2), 2.5, Some(10.0)));
        assert!(shows(&(50..60), 0.0, None), "not laid out yet");
        assert!(shows(&(50..60), 0.0, Some(0.0)), "laid out without height");
        assert!(
            !shows(&(5..5), 0.0, Some(10.0)),
            "an empty hunk never shows"
        );
    }

    #[test]
    fn reveal_keeps_a_margin_and_leaves_visible_rows_alone() {
        // Already comfortably visible.
        assert_eq!(reveal(10.0, 40.0, 20.0, 22.0, 6.0), None);
        // Below the viewport: scroll so the row sits a margin above the bottom.
        assert_eq!(reveal(10.0, 40.0, 48.0, 50.0, 6.0), Some(16.0));
        // Above: a margin below the top, never past zero.
        assert_eq!(reveal(10.0, 40.0, 12.0, 14.0, 6.0), Some(6.0));
        assert_eq!(reveal(10.0, 40.0, 2.0, 4.0, 6.0), Some(0.0));
    }
}
