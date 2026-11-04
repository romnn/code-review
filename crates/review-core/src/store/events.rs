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
    /// A note was attached to a unit.
    Note(NoteEvent),
    /// A walkthrough was imported.
    Walkthrough(WalkthroughEvent),
    /// The reviewer decided whether tools may execute code from the reviewed revisions.
    Trust(TrustEvent),
}

/// A reviewer's trust decision; the latest one in the log applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustEvent {
    /// Whether tools that can execute code from the reviewed revisions, such as language servers
    /// running Cargo, may run.
    pub trusted: bool,
    /// Seconds since the Unix epoch.
    pub at: u64,
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
