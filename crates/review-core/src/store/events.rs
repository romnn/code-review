//! The events recorded in a review's log.

use serde::{Deserialize, Serialize};

use crate::ids::{ObjectId, RevisionNumber, UnitIndex};

/// One record of the event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A revision was observed and is current.
    /// Its number is assigned under the lock, in log order; a snapshot pair seen before gets a new
    /// number unless it is the current revision's.
    Revision(RevisionEvent),
    /// A unit was marked.
    Mark(MarkEvent),
    /// One change of a unit that changed since it was reviewed was approved, leaving others.
    ///
    /// It has a tag of its own, never a mark's with an extra field, so a binary that predates
    /// it fails to read the log rather than reading it as the whole unit accepted.
    PartialApproval(PartialApprovalEvent),
    /// A note was attached to a unit.
    Note(NoteEvent),
    /// A walkthrough was imported.
    Walkthrough(WalkthroughEvent),
    /// The reviewer decided whether tools may run the code in the checkout.
    Trust(TrustEvent),
}

/// A reviewer's trust decision; in the review of a checkout, the latest one of scope
/// [`TrustScope::InCheckout`] applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEvent {
    /// Whether tools that run the code, such as language servers building it, may run: a grant,
    /// or a revocation of the grant of the same scope.
    pub trusted: bool,
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// What the decision covers; `None` is a whole review's, as logs written before scopes
    /// hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<TrustScope>,
}

/// What a trust decision covers.
///
/// Only [`Self::InCheckout`] grants anything.
/// The other scopes are read from logs whose grants were confirmed for tools running in
/// read-only copies of the code, with builds off, which no grant to run in the checkout itself
/// may stand in for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrustScope {
    /// Exactly one captured snapshot, by its tree, in a copy of its files.
    Snapshot {
        /// The snapshot's tree.
        tree: ObjectId,
    },
    /// Whatever the checkout holds, in copies of its files, for as long as it is the same clone.
    Checkout,
    /// Tools running in the checkout itself, with builds on, whatever it holds after later edits
    /// and branch switches, for as long as it is the same clone.
    InCheckout,
}

/// A newly observed revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionEvent {
    /// The revision's number, one more than the previous revision's.
    pub number: RevisionNumber,
    /// The base snapshot: the first merge base of the target and the head.
    pub base: ObjectId,
    /// The head snapshot.
    pub head: ObjectId,
    /// The target branch commit the base was computed from, when there is one.
    pub target: Option<ObjectId>,
    /// Every merge base git reported; more than one is reported to the user.
    pub merge_bases: Vec<ObjectId>,
    /// Seconds since the Unix epoch.
    pub recorded_at: u64,
    /// The branch checked out when a capture of a working tree was taken, if one was.
    ///
    /// Like [`Self::head_commit`] and [`Self::pull_request`], it only describes the revision:
    /// the snapshot pair is its identity, so the same pair seen on another branch is the same
    /// revision, and keeps the branch it was first recorded on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit `HEAD` was at when a capture of a working tree was taken, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_commit: Option<ObjectId>,
    /// The number of the branch's pull request when a capture of a working tree was taken, if
    /// one was known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<u64>,
}

/// A reviewer's verdict on a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkVerdict {
    /// Looked at.
    Seen,
    /// Approved.
    Accepted,
    /// Needs attention.
    Flagged,
    /// Requires review again; stops inheritance from earlier marks.
    Unseen,
}

impl MarkVerdict {
    /// Returns the verdict's name as used on the command line.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Seen => "seen",
            Self::Accepted => "accepted",
            Self::Flagged => "flagged",
            Self::Unseen => "unseen",
        }
    }
}

impl std::str::FromStr for MarkVerdict {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "seen" => Ok(Self::Seen),
            "accepted" => Ok(Self::Accepted),
            "flagged" => Ok(Self::Flagged),
            "unseen" => Ok(Self::Unseen),
            _ => Err(format!(
                "unknown verdict {text:?}, expected seen, accepted, flagged or unseen"
            )),
        }
    }
}

/// A mark on one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkEvent {
    /// The revision the mark was made on.
    pub revision: RevisionNumber,
    /// The unit within that revision.
    pub unit: UnitIndex,
    /// The digest of the unit's full evidence when it was marked.
    pub evidence: String,
    /// The verdict.
    pub verdict: MarkVerdict,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

/// The approval of one change of a unit, recorded as the reviewed text it leaves.
///
/// The text is the unit's head lines as approved so far: what it was reviewed as, with the
/// approved change's current lines in place of its reviewed ones.
/// The unit's other changes compare against it until a whole mark replaces it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartialApprovalEvent {
    /// The revision the approval was made on.
    pub revision: RevisionNumber,
    /// The unit within that revision.
    pub unit: UnitIndex,
    /// The digest of the unit's full evidence when it was approved.
    pub evidence: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
    /// The unit's head lines as approved so far, each with its terminator.
    pub text: String,
}

/// A note on one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteEvent {
    /// The revision the note was written on.
    pub revision: RevisionNumber,
    /// The unit within that revision.
    pub unit: UnitIndex,
    /// The digest of the unit's full evidence when the note was written.
    pub evidence: String,
    /// The note text.
    pub text: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

/// A validated walkthrough, stored with the evidence of every unit it references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkthroughEvent {
    /// The revision the walkthrough was written for.
    pub revision: RevisionNumber,
    /// The walkthrough's title.
    pub title: String,
    /// The ordered steps.
    pub steps: Vec<StoredStep>,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

/// One stored walkthrough step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredStep {
    /// The step's title.
    pub title: String,
    /// Why the step matters.
    pub why: String,
    /// The units of the walkthrough's revision in this step.
    pub units: Vec<StoredUnit>,
}

/// A unit reference with the digest of its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredUnit {
    /// The unit within the walkthrough's revision.
    pub unit: UnitIndex,
    /// The digest of the unit's full evidence.
    pub evidence: String,
}
