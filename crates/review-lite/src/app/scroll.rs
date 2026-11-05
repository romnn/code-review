//! Smooth scrolling: eased, retargetable scroll animations, and the scroll handle the
//! scrollbars use so that clicking the track pages.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{Bounds, Pixels, Point, Size, UniformListScrollHandle, point, px};
use gpui_component::scroll::ScrollbarHandle;

/// Converts a row count or index to a float for pixel arithmetic.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "row counts stay far below 2^24, where f32 is exact"
)]
pub fn float(count: usize) -> f32 {
    count as f32
}

/// The whole number of rows in a non-negative pixel distance, rounded down.
#[must_use]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to zero first, and row counts fit in usize"
)]
pub fn whole(value: f32) -> usize {
    value.max(0.0).floor() as usize
}

/// How long a programmatic scroll takes.
pub const DURATION: Duration = Duration::from_millis(150);

/// A cubic ease-out over `t` in `0..=1`: fast at first, settling gently.
#[must_use]
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// A scroll animation of one axis, in positive offsets (distance from the top).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animation {
    from: f32,
    to: f32,
    started: Instant,
}

impl Animation {
    /// Starts an animation from `from` to `to` at `now`.
    #[must_use]
    pub fn new(from: f32, to: f32, now: Instant) -> Self {
        Self {
            from,
            to,
            started: now,
        }
    }

    /// The offset at `now`.
    #[must_use]
    pub fn value_at(&self, now: Instant) -> f32 {
        let t = now.saturating_duration_since(self.started).as_secs_f32() / DURATION.as_secs_f32();
        self.from + (self.to - self.from) * ease_out(t)
    }

    /// Whether the animation has arrived at `now`.
    #[must_use]
    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= DURATION
    }

    /// Where the animation is heading.
    #[must_use]
    pub fn target(&self) -> f32 {
        self.to
    }

    /// Sends the animation to `to` from wherever it is at `now`, so a new jump continues the
    /// motion instead of restarting it.
    pub fn retarget(&mut self, to: f32, now: Instant) {
        *self = Self::new(self.value_at(now), to, now);
    }
}

/// The offset that brings `top..bottom` into a viewport of `height` at offset `current`,
/// keeping `margin` around it where it fits, or `None` when it is already comfortably visible.
#[must_use]
pub fn reveal(current: f32, height: f32, top: f32, bottom: f32, margin: f32) -> Option<f32> {
    let margin = margin.min(((height - (bottom - top)) / 2.0).max(0.0));
    if top - margin < current {
        Some((top - margin).max(0.0))
    } else if bottom + margin > current + height {
        Some((bottom + margin - height).max(0.0))
    } else {
        None
    }
}

/// A uniform list's scroll handle for the scrollbars: dragging the thumb scrolls directly, and
/// a click on the track asks for a page step instead of jumping there.
#[derive(Clone)]
pub struct PagedScroll {
    list: UniformListScrollHandle,
    dragging: Rc<Cell<bool>>,
    page_request: Rc<Cell<Option<f32>>>,
}

impl PagedScroll {
    /// Wraps the handle of the list the scrollbar belongs to.
    #[must_use]
    pub fn new(list: &UniformListScrollHandle) -> Self {
        Self {
            list: list.clone(),
            dragging: Rc::default(),
            page_request: Rc::default(),
        }
    }

    /// Takes the vertical offset a track click asked for, if any.
    #[must_use]
    pub fn take_page_request(&self) -> Option<f32> {
        self.page_request.take()
    }
}

impl ScrollbarHandle for PagedScroll {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.list.viewport_bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        ScrollbarHandle::offset(&self.list)
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let current = ScrollbarHandle::offset(&self.list);
        if self.dragging.get() || offset.y == current.y {
            ScrollbarHandle::set_offset(&self.list, offset);
            return;
        }
        // A track click: move one page toward the click, keeping a row of context.
        let page = (f32::from(self.viewport_bounds().size.height) - 40.0).max(40.0);
        let from = -f32::from(current.y);
        let toward = -f32::from(offset.y);
        let target = if toward > from {
            from + page
        } else {
            from - page
        };
        let max = f32::from(self.content_size().height - self.viewport_bounds().size.height);
        ScrollbarHandle::set_offset(&self.list, point(offset.x, current.y));
        self.page_request.set(Some(target.clamp(0.0, max.max(0.0))));
    }

    fn content_size(&self) -> Size<Pixels> {
        self.list.content_size()
    }

    fn start_drag(&self) {
        self.dragging.set(true);
    }

    fn end_drag(&self) {
        self.dragging.set(false);
    }
}

/// The vertical offset of a list, as a positive distance from the top.
#[must_use]
pub fn offset_of(list: &UniformListScrollHandle) -> f32 {
    -f32::from(ScrollbarHandle::offset(list).y)
}

/// Sets the vertical offset of a list, keeping its horizontal offset.
pub fn set_offset_of(list: &UniformListScrollHandle, offset: f32) {
    let current = ScrollbarHandle::offset(list);
    ScrollbarHandle::set_offset(list, point(current.x, px(-offset)));
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
        let mut animation = Animation::new(0.0, 100.0, start);
        let mid = start + DURATION / 2;
        let before = animation.value_at(mid);
        animation.retarget(300.0, mid);
        assert!((animation.value_at(mid) - before).abs() < 0.001);
        assert!(!animation.done(mid));
        assert!((animation.value_at(mid + DURATION) - 300.0).abs() < 0.001);
        assert!(animation.done(mid + DURATION));
    }

    #[test]
    fn reveal_keeps_a_margin_and_leaves_visible_rows_alone() {
        // Already comfortably visible.
        assert_eq!(reveal(100.0, 400.0, 200.0, 220.0, 60.0), None);
        // Below the viewport: scroll so the row sits a margin above the bottom.
        assert_eq!(reveal(100.0, 400.0, 480.0, 500.0, 60.0), Some(160.0));
        // Above: a margin below the top, never past zero.
        assert_eq!(reveal(100.0, 400.0, 120.0, 140.0, 60.0), Some(60.0));
        assert_eq!(reveal(100.0, 400.0, 20.0, 40.0, 60.0), Some(0.0));
    }
}
