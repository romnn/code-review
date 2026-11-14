//! The time one text's highlighting may take.

use std::time::{Duration, Instant};

/// How long highlighting one blob may take, parsing, injections, queries and bat's syntaxes
/// together.
///
/// Highlighting runs in the background one blob after another, so this bounds how long one
/// pathological file holds up the rest, while a release build still highlights a MiB or two of
/// ordinary code or generated JSON within it.
/// Whatever is not highlighted when it runs out stays plain.
pub const BUDGET: Duration = Duration::from_secs(2);

/// The moment one text's highlighting must stop.
///
/// review-core's scope parse stops at it too, given it as an instant.
/// tree-house stops at it by itself: its parse, injections and locals take the time left as
/// their timeout, and its highlighter's queries a `tree_house::Budget` of the time left after
/// them.
/// syntect's line loop checks it cooperatively between lines, so syntect stops only after the
/// line it is on.
#[derive(Debug, Clone, Copy)]
pub(super) struct Deadline {
    at: Instant,
}

impl Deadline {
    /// The deadline `budget` from now.
    pub(super) fn after(budget: Duration) -> Self {
        let now = Instant::now();
        Self {
            at: now.checked_add(budget).unwrap_or(now),
        }
    }

    /// Whether the deadline has passed.
    pub(super) fn expired(self) -> bool {
        Instant::now() >= self.at
    }

    /// The time left until the deadline, zero once it has passed.
    pub(super) fn remaining(self) -> Duration {
        self.at.saturating_duration_since(Instant::now())
    }

    /// The moment of the deadline.
    pub(super) fn instant(self) -> Instant {
        self.at
    }

    /// This deadline, or `budget` from now when that comes first.
    pub(super) fn capped(self, budget: Duration) -> Self {
        Self {
            at: self.at.min(Self::after(budget).at),
        }
    }
}
