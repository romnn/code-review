//! The histories a review last read, kept in memory, so a review that reads its history again
//! and again, as a live working-tree review does on every capture, derives only what is new.

use std::sync::{Arc, Mutex, PoisonError};

use crate::analysis::Analysis;
use crate::error::Error;
use crate::history::{History, Links};
use crate::ids::ObjectId;
use crate::store::ReviewDir;
use crate::store::log::LogTail;

/// A history of the recorded revisions, with the reader of the log just past the last record
/// it applied.
#[derive(Debug, Clone)]
pub(super) struct Replayed {
    pub history: History,
    pub tail: LogTail,
}

impl Replayed {
    /// The history of nothing recorded, with a reader at the start of the log of the review in
    /// `dir`.
    pub(super) fn start(dir: &ReviewDir) -> Self {
        Self {
            history: History::empty(),
            tail: LogTail::new(dir),
        }
    }
}

/// What the memo keeps.
#[derive(Debug, Clone, Default)]
struct Kept {
    replayed: Option<Arc<Replayed>>,
    /// The last history read with a provisional revision on top of a recorded one.
    live: Option<Arc<History>>,
    /// Whether the log was found replaced, which ends the review for every clone.
    replaced: bool,
}

/// The memo every clone of a review shares.
///
/// Its lock is held only to take or put whole histories, which are shared, not copied; clones
/// read their histories outside it, so a slow read never holds up another, and two clones
/// reading at once may each derive the same records, which only costs deriving them twice.
#[derive(Debug, Clone, Default)]
pub(super) struct Memo {
    kept: Arc<Mutex<Kept>>,
}

impl Memo {
    /// Returns the history of the recorded revisions read last, with the reader of the log at
    /// its version.
    pub(super) fn replayed(&self) -> Option<Arc<Replayed>> {
        self.lock().replayed.clone()
    }

    /// Keeps `replayed` unless a history of more of the log is kept.
    pub(super) fn keep_replayed(&self, replayed: Arc<Replayed>) {
        let mut kept = self.lock();
        if kept
            .replayed
            .as_ref()
            .is_none_or(|kept| kept.history.version() <= replayed.history.version())
        {
            kept.replayed = Some(replayed);
        }
    }

    /// Returns the last history read with a provisional revision on top.
    pub(super) fn live(&self) -> Option<Arc<History>> {
        self.lock().live.clone()
    }

    /// Keeps `history`, which ends in a provisional revision, in place of the last one.
    pub(super) fn keep_live(&self, history: Arc<History>) {
        self.lock().live = Some(history);
    }

    /// Whether a clone found the log replaced; see [`Error::LogReplaced`].
    pub(super) fn replaced(&self) -> bool {
        self.lock().replaced
    }

    /// Notes that `result` found the log replaced, if it did, for every clone.
    pub(super) fn note_replaced<T>(&self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(Error::LogReplaced { .. }) = &result {
            self.lock().replaced = true;
        }
        result
    }

    /// Returns the analysis of `base` against `head` that a kept history has.
    pub(super) fn analysis(&self, base: &ObjectId, head: &ObjectId) -> Option<Arc<Analysis>> {
        let kept = self.lock().clone();
        kept.histories()
            .find_map(|history| history.analysis_of(base, head))
            .map(Arc::clone)
    }

    /// Returns the links between consecutive revisions analyzed as `older` and `newer` that a
    /// kept history has.
    pub(super) fn links(&self, older: &Analysis, newer: &Analysis) -> Option<Arc<Links>> {
        let kept = self.lock().clone();
        kept.histories()
            .find_map(|history| history.links_between(older, newer))
            .map(Arc::clone)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Kept> {
        // A panic while holding the memo leaves at worst a history read before the latest,
        // which a reader extends by the records it lacks.
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Kept {
    fn histories(&self) -> impl Iterator<Item = &History> {
        self.live
            .as_deref()
            .into_iter()
            .chain(self.replayed.as_deref().map(|replayed| &replayed.history))
    }
}
