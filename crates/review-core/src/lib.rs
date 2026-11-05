//! Review engine shared by the `cr` CLI and the desktop front ends.
//!
//! The engine remembers what a reviewer has seen down to the review unit, across pushes, rebases
//! and force-pushes.
//! Its overriding rule: it never makes a reviewer believe they reviewed code they did not.
//! Whenever identity, equivalence or a move is ambiguous, it reopens the code instead of guessing.
//!
//! The API is synchronous; front ends call it from their own background threads.
//!
//! - [`Review`] opens a review, records revisions, and performs every state transition (marks,
//!   notes, walkthrough imports) under the review's lock.
//! - [`Review::history`] loads a [`History`]: every revision with its [`analysis`], the
//!   [`correspond`]ences between consecutive revisions, and derived unit states.
//! - [`History::view`] gives a [`RevisionView`] with unit states, interdiffs, notes, moves and
//!   progress counts.
//! - [`walkthrough::partition`] orders a revision's units into steps, and [`export::export`]
//!   collects flagged, accepted and annotated units for an agent.
//!
//! # Examples
//!
//! ```no_run
//! # // Uses `no_run` because it needs a git repository with a `main` and a `feature` branch.
//! # fn main() -> Result<(), review_core::Error> {
//! use std::path::Path;
//!
//! use review_core::{MarkVerdict, Repo, Review, ReviewTarget};
//!
//! let repo = Repo::discover(Path::new("."))?;
//! let target = ReviewTarget::parse("main..feature", &repo)?;
//! let review = Review::open(Path::new("/var/lib/code-review"), repo, target)?;
//! review.refresh()?;
//! let history = review.history()?;
//! let view = history.current_view()?;
//! let unreviewed: Vec<_> = view
//!     .unit_ids()
//!     .into_iter()
//!     .filter(|&unit| view.state(unit).is_ok_and(|state| !state.is_reviewed()))
//!     .collect();
//! review.mark(&view, &unreviewed, MarkVerdict::Seen)?;
//! # Ok(())
//! # }
//! ```

pub mod analysis;
pub mod correspond;
pub mod demo;
mod error;
pub mod export;
pub mod git;
mod history;
mod ids;
mod review;
pub mod scope;
pub mod store;
pub mod syntax;
mod text;
pub mod tree;
mod view;
pub mod walkthrough;

pub use error::Error;
pub use git::Repo;
pub use history::{History, RevisionRecord, UnitState, Verdict};
pub use ids::{MoveId, ObjectId, ParseIdError, ReviewKey, RevisionNumber, UnitId, UnitIndex};
pub use review::{
    Commit, PullRequest, Recorded, Review, ReviewInfo, ReviewMeta, ReviewTarget, RevisionInput,
    Trust,
};
pub use store::events::MarkVerdict;
pub use view::{
    ChangeKind, Collapse, Counts, DiffLine, DiffLineKind, FileChangeSummary, FileStatus, FileTone,
    Interdiff, LineEnding, Location, MoveEnd, MoveView, NoteAttachment, ResolvedNote, RevisionView,
    ScopeAnchor, Side, SideDiff, SplitLine, Status, split_line_ending,
};
