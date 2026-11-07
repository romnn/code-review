//! Where each unit stands with the reviewer: its review state, with proven formatting-only units
//! approved automatically when the review's policy says so.
//!
//! Automatic approval never writes a mark.
//! It is derived anew for every revision from that revision's formatting proof, so a unit that
//! stops being formatting-only needs review again, and an explicit mark anywhere on a unit's
//! chain always decides instead: a flag keeps it flagged, an `unseen` mark makes it need review,
//! and a change since a review shows that change.

use serde::Serialize;

use crate::history::{UnitState, Verdict};

/// How a view decides which units still need the reviewer.
///
/// It has no default on purpose: a front end that approves anything automatically states so
/// when it builds a view, with [`History::view_under`](crate::History::view_under), and every
/// other view decides by marks alone, under [`Self::MARKS_ONLY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReviewPolicy {
    /// Whether a unit whose change is proven formatting-only, and that no mark on its chain
    /// decides, counts as approved without one.
    pub auto_approve_formatting: bool,
}

impl ReviewPolicy {
    /// Only the reviewer's marks decide: nothing is approved automatically.
    pub const MARKS_ONLY: Self = Self {
        auto_approve_formatting: false,
    };
}

/// Where a unit stands with the reviewer at one revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "standing", content = "verdict")]
pub enum Standing {
    /// Needs the reviewer: never reviewed and not approved automatically, or explicitly marked
    /// `unseen` on its chain.
    Open,
    /// Approved without a mark: its change is proven formatting-only and no mark on its chain
    /// decides it.
    AutoApproved,
    /// Reviewed with this verdict, here or carried along unchanged links.
    Reviewed(Verdict),
    /// Reviewed before, but the change differs since, or its identity is uncertain.
    Changed,
}

impl Standing {
    /// Where a unit in `state` stands, whose change is proven formatting-only when
    /// `formatting_only`, under `policy`.
    ///
    /// Only a unit with no mark on its chain ([`UnitState::New`]) is approved automatically;
    /// any mark decides instead.
    #[must_use]
    pub fn of(state: UnitState, formatting_only: bool, policy: ReviewPolicy) -> Self {
        match state {
            UnitState::New if formatting_only && policy.auto_approve_formatting => {
                Self::AutoApproved
            }
            UnitState::New | UnitState::Unseen { .. } => Self::Open,
            UnitState::Reviewed { verdict, .. } => Self::Reviewed(verdict),
            UnitState::ChangedSinceReviewed { .. } => Self::Changed,
        }
    }

    /// Whether the unit still needs the reviewer: open, or changed since it was reviewed.
    #[must_use]
    pub fn needs_review(self) -> bool {
        matches!(self, Self::Open | Self::Changed)
    }

    /// Whether the unit counts as reviewed: marked with any verdict, flagged included, or
    /// approved automatically.
    #[must_use]
    pub fn is_reviewed(self) -> bool {
        !self.needs_review()
    }

    /// Whether the unit counts as approved: seen, accepted or approved automatically.
    #[must_use]
    pub fn is_approved(self) -> bool {
        matches!(
            self,
            Self::AutoApproved | Self::Reviewed(Verdict::Seen | Verdict::Accepted)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{RevisionNumber, UnitId, UnitIndex};

    fn unit() -> UnitId {
        UnitId::new(RevisionNumber::FIRST, UnitIndex::new(0))
    }

    /// A proven formatting-only unit without a mark is approved automatically, and only then;
    /// every mark on its chain decides instead, and the policy can turn it off.
    #[test]
    fn only_unmarked_formatting_only_units_are_approved_automatically() {
        let on = ReviewPolicy {
            auto_approve_formatting: true,
        };
        let off = ReviewPolicy::MARKS_ONLY;
        assert_eq!(
            Standing::of(UnitState::New, true, on),
            Standing::AutoApproved
        );
        assert_eq!(Standing::of(UnitState::New, false, on), Standing::Open);
        assert_eq!(Standing::of(UnitState::New, true, off), Standing::Open);
        // An explicit reset makes it need review again.
        let unseen = UnitState::Unseen { marked: unit() };
        assert_eq!(Standing::of(unseen, true, on), Standing::Open);
        // A flag keeps it flagged.
        let flagged = UnitState::Reviewed {
            verdict: Verdict::Flagged,
            marked: unit(),
            inherited: false,
        };
        assert_eq!(
            Standing::of(flagged, true, on),
            Standing::Reviewed(Verdict::Flagged)
        );
        // A change since a review shows that change rather than approving it.
        let changed = UnitState::ChangedSinceReviewed {
            verdict: Verdict::Accepted,
            reviewed: unit(),
        };
        assert_eq!(Standing::of(changed, true, on), Standing::Changed);
    }

    /// Automatic approval counts as approved and reviewed; a flag is reviewed but not
    /// approved.
    #[test]
    fn standings_say_what_needs_the_reviewer() {
        assert!(Standing::AutoApproved.is_approved());
        assert!(Standing::AutoApproved.is_reviewed());
        assert!(!Standing::Reviewed(Verdict::Flagged).is_approved());
        assert!(Standing::Reviewed(Verdict::Flagged).is_reviewed());
        assert!(Standing::Open.needs_review());
        assert!(Standing::Changed.needs_review());
    }
}
