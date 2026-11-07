//! The overview's account of the review's revisions.

use review_core::{History, ReviewPolicy};

/// One revision in the overview's history.
#[derive(Debug, Clone)]
pub struct RevisionSummary {
    /// The revision number.
    pub number: String,
    /// When it was recorded.
    pub recorded_at: u64,
    /// The head commit, abbreviated.
    pub head: String,
    /// Units in the revision.
    pub units: usize,
    /// Units never reviewed.
    pub new: usize,
    /// Units changed since they were reviewed.
    pub changed: usize,
    /// Units reviewed, carried or marked here, or approved automatically.
    pub reviewed: usize,
}

/// Summarizes every revision of `history`, deciding under `policy` which units need the
/// reviewer.
pub(crate) fn summarize(history: &History, policy: ReviewPolicy) -> Vec<RevisionSummary> {
    history
        .revisions()
        .into_iter()
        .filter_map(|record| {
            let total = history
                .view_under(record.number, policy)
                .ok()?
                .status()
                .total;
            Some(RevisionSummary {
                number: record.number.to_string(),
                recorded_at: record.recorded_at,
                head: record.head.as_str().chars().take(10).collect(),
                units: total.units,
                new: total.new,
                changed: total.changed,
                reviewed: total.done(),
            })
        })
        .collect()
}
