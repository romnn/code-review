//! The analyses and correspondences the latest history used, kept in memory, so a review that
//! reads its history again and again, as a live working-tree review does on every change,
//! decodes only what is new.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use crate::analysis::Analysis;
use crate::correspond::Correspondence;
use crate::ids::ObjectId;

/// A snapshot pair: base and head.
pub(super) type Pair = (ObjectId, ObjectId);

/// What one history used, by snapshot pair.
#[derive(Debug, Default)]
struct Kept {
    analyses: HashMap<Pair, Arc<Analysis>>,
    correspondences: HashMap<(Pair, Pair), Arc<Correspondence>>,
}

/// The memo every clone of a review shares.
#[derive(Debug, Clone, Default)]
pub(super) struct Memo {
    kept: Arc<Mutex<Kept>>,
}

impl Memo {
    pub(super) fn analysis(&self, pair: &Pair) -> Option<Arc<Analysis>> {
        self.lock().analyses.get(pair).cloned()
    }

    pub(super) fn correspondence(&self, older: &Pair, newer: &Pair) -> Option<Arc<Correspondence>> {
        self.lock()
            .correspondences
            .get(&(older.clone(), newer.clone()))
            .cloned()
    }

    /// Keeps exactly what a history just used: `analyses` by pair, and the correspondences
    /// between consecutive ones.
    ///
    /// Whatever no history uses any more goes, so the memo never holds more than one history's
    /// worth; two clones building histories at once may each drop the other's entries, which
    /// only costs decoding them again.
    pub(super) fn keep(
        &self,
        analyses: &[(Pair, Arc<Analysis>)],
        correspondences: &[Arc<Correspondence>],
    ) {
        let mut kept = self.lock();
        let pairs: HashSet<&Pair> = analyses.iter().map(|(pair, _)| pair).collect();
        kept.analyses.retain(|pair, _| pairs.contains(pair));
        kept.correspondences.clear();
        for (pair, analysis) in analyses {
            kept.analyses
                .entry(pair.clone())
                .or_insert_with(|| Arc::clone(analysis));
        }
        for (window, correspondence) in analyses.windows(2).zip(correspondences) {
            if let [(older, _), (newer, _)] = window {
                kept.correspondences
                    .insert((older.clone(), newer.clone()), Arc::clone(correspondence));
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Kept> {
        // A panic while holding the memo leaves at worst entries a history can use, since each
        // is inserted whole.
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
