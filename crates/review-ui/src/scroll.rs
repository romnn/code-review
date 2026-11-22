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

/// The share of the viewport kept clear above and below a hunk the keys go to, as vim's
/// `scrolloff` keeps lines around the cursor, so the code around it shows and a long hunk
/// shows from its start this far down.
pub const READING_LINE: f64 = 0.2;

/// The offset that shows rows `top..bottom` in a viewport `height` rows tall at offset
/// `current` from their start `share` of the viewport down, or `None` when they already show
/// with that much clear above and below them.
///
/// Whichever way the keys go, a hunk lands with its start at that reading line, so the code
/// before it shows, the hunk shows whole as far as the viewport allows, and so does what
/// follows it; a hunk already shown so stays put, so short steps never jump. A view that would
/// leave no more than the margin above it goes to the very top instead, never hiding a sliver
/// of a file's start.
#[must_use]
pub fn within_margins(current: f64, height: f64, top: f64, bottom: f64, share: f64) -> Option<f64> {
    let margin = (height * share).floor();
    let shown = top >= current + margin && bottom <= current + height - margin;
    if shown {
        return None;
    }
    let target = reading(height, top, share);
    ((target - current).abs() > f64::EPSILON).then_some(target)
}

/// The offset that shows rows from `top` at the reading line `share` of a viewport `height`
/// rows tall down, or from the very top when that would hide no more than the rows above the
/// reading line, so a file's start never shows cut by a sliver.
#[must_use]
pub fn reading(height: f64, top: f64, share: f64) -> f64 {
    let margin = (height * share).floor();
    let target = top - margin;
    if target <= margin { 0.0 } else { target }
}

/// The offset that shows rows `top..bottom` in the middle of a viewport `height` rows tall,
/// whole rows from the top, unless that would leave less than `share` of the viewport above
/// them: rows too tall for the middle show from their start at the reading line instead.
#[must_use]
pub fn centered(height: f64, top: f64, bottom: f64, share: f64) -> f64 {
    let above = (height * share).floor();
    let middle = f64::midpoint(top, bottom) - height / 2.0;
    middle.min(top - above).max(0.0).floor()
}

/// How quickly the wheel's momentum fades once the wheel slows or stops: the time in which
/// it falls to about a third.
const MOMENTUM_FADE: Duration = Duration::from_millis(300);

/// How close together wheel events count as one notch: a terminal sends several events for one
/// notch at once, and a desktop wheel one.
const NOTCH_BURST: Duration = Duration::from_millis(4);

/// The notches a second past which the wheel builds momentum, so turning it at an easy pace
/// scrolls exactly as far as its notches say.
const MOMENTUM_ONSET: f64 = 6.0;

/// The most the wheel's momentum multiplies a notch by.
const MOMENTUM_MAX: f64 = 12.0;

/// The momentum a mouse wheel builds as it keeps turning fast, as a simple physics model.
///
/// Every notch adds one to an energy that fades exponentially with time, so the energy tracks
/// how fast the wheel turns, in notches a second, whatever rows a notch scrolls and however
/// many events a terminal sends for it.
/// A notch scrolls by the momentum the notches before it built, so a burst the platform
/// delivered as one scrolls as far as it says.
/// Up to [`MOMENTUM_ONSET`] a notch scrolls as far as it says, so short and slow turns stay
/// precise; past it, each notch scrolls further with the square of how fast the wheel keeps
/// turning, so a quick spin travels far, up to [`MOMENTUM_MAX`] times as far.
/// What momentum adds is whole rows, the fraction carried to the next notch, so a notch of
/// whole rows keeps the list at a whole row, as a terminal draws it.
/// Turning the other way, or another list, starts from rest.
/// Nothing glides on after the wheel stops: the rows scroll as the notches arrive.
#[derive(Debug, Clone, Copy, Default)]
pub struct WheelMomentum {
    /// The faded count of the notches the wheel turned.
    energy: f64,
    /// The part of a row momentum added that the notches so far left over, which the next
    /// notch the same way scrolls.
    carry: f64,
    /// The last notch, which the next one builds on.
    last: Option<Notch>,
}

/// A notch of the wheel, which the next one builds on.
#[derive(Debug, Clone, Copy)]
struct Notch {
    /// When its first event arrived.
    at: Instant,
    list: crate::frame::List,
    down: bool,
    /// How many times as far its events scroll.
    factor: f64,
}

impl WheelMomentum {
    /// Takes in a wheel event of `rows` over `list` at `now`, down when positive, and returns
    /// how many rows it scrolls with the momentum built so far.
    pub fn turn(&mut self, list: crate::frame::List, rows: f64, now: Instant) -> f64 {
        if rows == 0.0 {
            return 0.0;
        }
        let down = rows > 0.0;
        let factor = match self.last {
            Some(notch) if notch.list == list && notch.down == down => {
                let elapsed = now.saturating_duration_since(notch.at);
                if elapsed < NOTCH_BURST {
                    // Another event of the same notch.
                    notch.factor
                } else {
                    self.energy = self.energy
                        * (-elapsed.as_secs_f64() / MOMENTUM_FADE.as_secs_f64()).exp()
                        + 1.0;
                    self.begin(list, down, now)
                }
            }
            _ => {
                *self = Self::default();
                self.energy = 1.0;
                self.begin(list, down, now)
            }
        };
        let exact = rows * (factor - 1.0) + self.carry;
        let extra = exact.trunc();
        self.carry = exact - extra;
        rows + extra
    }

    /// Starts a notch at `now`, scrolling by the momentum the notches before it built, and
    /// returns that factor.
    fn begin(&mut self, list: crate::frame::List, down: bool, now: Instant) -> f64 {
        // The notches before this one, at the rate they came.
        let rate = (self.energy - 1.0).max(0.0) / MOMENTUM_FADE.as_secs_f64();
        let factor = (rate / MOMENTUM_ONSET).powi(2).clamp(1.0, MOMENTUM_MAX);
        self.last = Some(Notch {
            at: now,
            list,
            down,
            factor,
        });
        factor
    }

    /// Drops the momentum, as when something other than the wheel scrolls the list.
    pub fn stop(&mut self) {
        *self = Self::default();
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

    /// A hunk lands with its start a fifth down whichever way the keys go, and one already
    /// shown with a fifth clear above and below stays put.
    #[test]
    fn hunks_land_a_fifth_down() {
        // Within the band from 8 to 32 rows down: no scroll.
        assert_eq!(within_margins(0.0, 40.0, 10.0, 14.0, 0.2), None);
        assert_eq!(within_margins(0.0, 40.0, 8.0, 32.0, 0.2), None);
        // Too close to the top: its start moves a fifth down.
        assert_eq!(within_margins(20.0, 40.0, 25.0, 28.0, 0.2), Some(17.0));
        // Near the very start of the file, at the top rather than a few rows down.
        assert_eq!(within_margins(4.0, 40.0, 5.0, 8.0, 0.2), Some(0.0));
        assert_eq!(within_margins(20.0, 40.0, 14.0, 18.0, 0.2), Some(0.0));
        // Too close to the bottom or below the viewport: its start too, so it and what follows
        // show.
        assert_eq!(within_margins(0.0, 40.0, 30.0, 34.0, 0.2), Some(22.0));
        assert_eq!(within_margins(0.0, 40.0, 100.0, 104.0, 0.2), Some(92.0));
        // A hunk taller than the band shows from its start there, and stays once it does,
        // never from its middle.
        assert_eq!(within_margins(0.0, 40.0, 50.0, 200.0, 0.2), Some(42.0));
        assert_eq!(within_margins(42.0, 40.0, 50.0, 200.0, 0.2), None);
        assert_eq!(within_margins(45.0, 40.0, 50.0, 200.0, 0.2), Some(42.0));
    }

    /// A hunk shows in the middle, or from its start at the reading line when it is too tall
    /// for the middle.
    #[test]
    fn centered_puts_a_hunk_in_the_middle() {
        assert!((centered(40.0, 100.0, 104.0, 0.25) - 82.0).abs() < f64::EPSILON);
        // Too tall for the middle: its start at the reading line.
        assert!((centered(40.0, 100.0, 130.0, 0.25) - 90.0).abs() < f64::EPSILON);
        assert!(
            (centered(40.0, 2.0, 4.0, 0.25)).abs() < f64::EPSILON,
            "never above the top"
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
    /// Notches `apart` apart over the diff, all down, and the rows the last one scrolls.
    fn spin(notches: usize, apart: Duration) -> f64 {
        let start = Instant::now();
        let mut momentum = WheelMomentum::default();
        let mut last = 0.0;
        for notch in 0..notches {
            let at = start + apart * u32::try_from(notch).unwrap();
            last = momentum.turn(crate::frame::List::Diff, 3.0, at);
        }
        last
    }

    /// An easy turn of the wheel scrolls exactly as far as its notches say, and a wheel kept
    /// spinning fast builds momentum up to a cap.
    #[test]
    fn a_fast_wheel_builds_momentum() {
        // One notch, and notches at five a second, scroll three rows each.
        assert!((spin(1, Duration::ZERO) - 3.0).abs() < f64::EPSILON);
        assert!((spin(20, Duration::from_millis(200)) - 3.0).abs() < f64::EPSILON);
        // Thirty notches a second scroll further, the more so the longer they keep coming.
        let early = spin(8, Duration::from_millis(33));
        let late = spin(15, Duration::from_millis(33));
        assert!(early > 3.0 && late > early, "{early} then {late}");
        // However fast the wheel spins, a notch scrolls at most twelve times as far.
        assert!((spin(200, Duration::from_millis(5)) - 36.0).abs() < f64::EPSILON);
    }

    /// Notches the platform delivers together scroll as far as they say, and what momentum
    /// adds is whole rows, so a list scrolled by whole rows stays at a whole row.
    #[test]
    fn momentum_comes_from_earlier_notches_in_whole_rows() {
        let start = Instant::now();
        let mut momentum = WheelMomentum::default();
        assert!((momentum.turn(crate::frame::List::Diff, 12.0, start) - 12.0).abs() < 1e-9);
        let mut burst = WheelMomentum::default();
        let rows: f64 = (0..4)
            .map(|_| burst.turn(crate::frame::List::Diff, 3.0, start))
            .sum();
        assert!((rows - 12.0).abs() < 1e-9, "{rows}");
        let mut terminal = WheelMomentum::default();
        for event in 0..60 {
            let rows = terminal.turn(
                crate::frame::List::Diff,
                1.0,
                start + Duration::from_millis(5 * event),
            );
            assert!((rows - rows.round()).abs() < 1e-9, "{rows}");
        }
    }

    /// Momentum fades once the wheel pauses, and turning back or over another list starts
    /// from rest.
    #[test]
    fn momentum_fades_and_resets() {
        let start = Instant::now();
        let fast = |momentum: &mut WheelMomentum| {
            for notch in 0..20 {
                momentum.turn(
                    crate::frame::List::Diff,
                    3.0,
                    start + Duration::from_millis(20 * notch),
                );
            }
        };
        let after = start + Duration::from_millis(400);
        // A second's pause leaves no momentum.
        let mut momentum = WheelMomentum::default();
        fast(&mut momentum);
        let rows = momentum.turn(
            crate::frame::List::Diff,
            3.0,
            after + Duration::from_secs(1),
        );
        assert!((rows - 3.0).abs() < 1e-9, "{rows}");
        // Turning the wheel back scrolls a notch's rows the other way.
        let mut momentum = WheelMomentum::default();
        fast(&mut momentum);
        let rows = momentum.turn(crate::frame::List::Diff, -3.0, after);
        assert!((rows + 3.0).abs() < f64::EPSILON, "{rows}");
        // Another list starts from rest.
        let mut momentum = WheelMomentum::default();
        fast(&mut momentum);
        let rows = momentum.turn(crate::frame::List::Tree, 3.0, after);
        assert!((rows - 3.0).abs() < f64::EPSILON, "{rows}");
    }

    /// A terminal's several events for one notch count as that one notch, so a wheel builds
    /// the same momentum in either front end: three one-row events at once scroll as far as
    /// one three-row event.
    #[test]
    fn a_notchs_events_count_once() {
        let start = Instant::now();
        let mut desktop = WheelMomentum::default();
        let mut terminal = WheelMomentum::default();
        let (mut desktop_rows, mut terminal_rows) = (0.0, 0.0);
        for notch in 0..20 {
            let at = start + Duration::from_millis(60 * notch);
            desktop_rows += desktop.turn(crate::frame::List::Diff, 3.0, at);
            for event in 0..3 {
                terminal_rows += terminal.turn(
                    crate::frame::List::Diff,
                    1.0,
                    at + Duration::from_micros(200 * event),
                );
            }
        }
        assert!(
            desktop_rows > 60.0,
            "sixteen notches a second build momentum"
        );
        assert!(
            (desktop_rows - terminal_rows).abs() <= 3.0,
            "{desktop_rows} and {terminal_rows}"
        );
    }
}
