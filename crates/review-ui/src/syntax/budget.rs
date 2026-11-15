//! The time one text's highlighting may take, and the cancellation that can end it sooner.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long highlighting one blob may take, parsing, injections, queries and bat's syntaxes
/// together.
///
/// Highlighting runs in the background one blob after another, so this bounds how long one
/// pathological file holds up the rest, while a release build still highlights a MiB or two of
/// ordinary code or generated JSON within it.
/// Whatever is not highlighted when it runs out stays plain.
pub const BUDGET: Duration = Duration::from_secs(2);

/// The moment one text's highlighting must stop, or sooner once its cancellation is set.
///
/// review-core's scope parse stops at it too, given it as an instant and the flag.
/// tree-house stops at it by itself: its parse, injections and locals take the time left as
/// their timeout and the flag, and its highlighter's queries a `tree_house::Budget` of the time
/// left after them and the flag.
/// syntect's line loop checks it cooperatively between lines, so syntect stops only after the
/// line it is on.
#[derive(Debug, Clone, Copy)]
pub(super) struct Deadline<'a> {
    at: Instant,
    cancel: Option<&'a AtomicBool>,
}

impl Deadline<'static> {
    /// The deadline `budget` from now, with nothing to cancel it.
    pub(super) fn after(budget: Duration) -> Self {
        let now = Instant::now();
        Self {
            at: now.checked_add(budget).unwrap_or(now),
            cancel: None,
        }
    }
}

impl<'a> Deadline<'a> {
    /// This deadline, ended early as well once `cancel` is set.
    pub(super) fn cancelled_by(self, cancel: Option<&'a AtomicBool>) -> Self {
        Self { cancel, ..self }
    }

    /// Whether the deadline has passed or the highlighting was cancelled.
    pub(super) fn expired(self) -> bool {
        self.is_cancelled() || Instant::now() >= self.at
    }

    /// The time left until the deadline, zero once it has passed or the highlighting was
    /// cancelled.
    pub(super) fn remaining(self) -> Duration {
        if self.is_cancelled() {
            return Duration::ZERO;
        }
        self.at.saturating_duration_since(Instant::now())
    }

    /// The moment of the deadline.
    pub(super) fn instant(self) -> Instant {
        self.at
    }

    /// The flag that cancels the highlighting, for the parts that check it themselves.
    pub(super) fn cancel(self) -> Option<&'a AtomicBool> {
        self.cancel
    }

    /// This deadline, or `budget` from now when that comes first.
    pub(super) fn capped(self, budget: Duration) -> Self {
        Self {
            at: self.at.min(Deadline::after(budget).at),
            cancel: self.cancel,
        }
    }

    fn is_cancelled(self) -> bool {
        self.cancel
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    }
}
