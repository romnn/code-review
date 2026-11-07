//! How fresh the review on screen is: which revision shows, when the review last synced with
//! its target, and what a fetch of the latest changes is doing, for the bottom bar.
//!
//! The bar's relative time and its spinner change with the clock alone, so timers redraw them:
//! one each minute while the review shows, and a quicker one only while a fetch runs.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use review_core::{RefreshStep, Synced};

use super::effects::{Because, Outcome, Subject};
use super::{Model, Phase};

/// How often the spinner turns while a fetch runs.
pub const SPIN: Duration = Duration::from_millis(100);

/// How long the bar says a fetch failed before it shows the last good sync again.
pub const FAILURE_SHOWS: Duration = Duration::from_secs(5);

/// What a fetch of the latest changes is doing, as its job reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStep {
    /// A step of the engine's refresh: fetching, analyzing a new revision, or describing it.
    Refresh(RefreshStep),
    /// Reading the history again, which carries the marks to a new revision.
    Carrying,
}

/// A timer that redraws what changes with the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tick {
    /// The spinner's next turn, while a fetch runs.
    Spin,
    /// The next minute of the time since the last sync; it keeps itself going.
    Minute,
    /// The end of a failure's moment on the bar.
    Recovered,
}

/// The review's freshness.
#[derive(Debug, Default)]
pub struct Sync {
    /// When the review last synced with its target, as the engine records it.
    pub synced: Option<Synced>,
    /// What the running fetch is doing; its job writes it from its own thread.
    pub(super) step: Arc<Mutex<Option<SyncStep>>>,
    /// When the last fetch failed, which the bar says for [`FAILURE_SHOWS`].
    pub failed_at: Option<SystemTime>,
}

impl Sync {
    /// What the running fetch is doing, or `None` when none runs.
    #[must_use]
    pub fn step(&self) -> Option<SyncStep> {
        *self.step.lock()
    }

    /// Whether the bar still says the last fetch failed at `now`.
    #[must_use]
    pub fn failing(&self, now: SystemTime) -> bool {
        self.failed_at.is_some_and(|at| {
            now.duration_since(at)
                .is_ok_and(|elapsed| elapsed < FAILURE_SHOWS)
        })
    }
}

/// Seconds since the Unix epoch at `now`, as the engine records times.
#[must_use]
pub fn epoch_seconds(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// How long ago `synced` happened, at `now`, as the bar says it: `just now`, `4m ago`,
/// `3h ago` or `2d ago`.
#[must_use]
pub fn ago(synced: &Synced, now: SystemTime) -> String {
    let seconds = epoch_seconds(now).saturating_sub(synced.at);
    match seconds {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

impl Model {
    /// Starts the clock that keeps the time since the last sync current, once the review
    /// shows.
    pub(super) fn start_clock(&mut self) {
        self.after(
            self.until_next_minute(),
            Subject::Always(Because::Clock),
            Outcome::Tick(Tick::Minute),
        );
    }

    /// How long until the time since the last sync reaches its next whole minute.
    fn until_next_minute(&self) -> Duration {
        let now = epoch_seconds(SystemTime::now());
        let into_minute = self
            .sync
            .synced
            .map_or(0, |synced| now.saturating_sub(synced.at) % 60);
        Duration::from_secs(60 - into_minute)
    }

    /// Turns the spinner while the review opens or a fetch runs; it stops by itself once they
    /// end.
    pub(super) fn spin(&mut self) {
        self.after(
            SPIN,
            Subject::Always(Because::Clock),
            Outcome::Tick(Tick::Spin),
        );
    }

    /// Takes in a tick of the clock: what shows the time redraws with the update it is, and a
    /// tick that keeps going asks for the next.
    pub(super) fn ticked(&mut self, tick: Tick) {
        match tick {
            Tick::Spin if self.refreshing || matches!(self.phase, Phase::Loading(_)) => {
                self.spin();
            }
            Tick::Minute => self.start_clock(),
            Tick::Spin | Tick::Recovered => {}
        }
    }

    /// Says on the bar that the last fetch failed, until [`FAILURE_SHOWS`] has passed.
    pub(super) fn sync_failed(&mut self) {
        self.sync.failed_at = Some(SystemTime::now());
        self.after(
            FAILURE_SHOWS,
            Subject::Always(Because::Clock),
            Outcome::Tick(Tick::Recovered),
        );
    }
}

#[cfg(test)]
mod tests {
    use review_core::RevisionNumber;

    use super::*;

    /// The time since a sync reads in its largest whole unit.
    #[test]
    fn the_time_since_a_sync_reads_in_whole_units() {
        let synced = Synced {
            at: 1_000_000,
            revision: RevisionNumber::FIRST,
        };
        let at = |seconds: u64| UNIX_EPOCH + Duration::from_secs(1_000_000 + seconds);
        assert_eq!(ago(&synced, at(59)), "just now");
        assert_eq!(ago(&synced, at(60)), "1m ago");
        assert_eq!(ago(&synced, at(3599)), "59m ago");
        assert_eq!(ago(&synced, at(3600)), "1h ago");
        assert_eq!(ago(&synced, at(2 * 86_400 + 5)), "2d ago");
        // A clock set back reads as just now rather than in the future.
        assert_eq!(ago(&synced, UNIX_EPOCH), "just now");
    }
}
