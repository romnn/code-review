//! The engine's error type.

use std::path::PathBuf;

use crate::ids::{MoveId, ObjectId, ParseIdError, RevisionNumber, UnitId};
use crate::walkthrough::WalkthroughError;

/// Errors returned by the review engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `git` or `gh` process could not be started.
    #[error("failed to run {program} {args}")]
    Spawn {
        /// The program, `git` or `gh`.
        program: &'static str,
        /// The arguments, joined by spaces.
        args: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A `git` or `gh` process exited unsuccessfully.
    #[error("{program} {args} failed: {stderr}")]
    CommandFailed {
        /// The program, `git` or `gh`.
        program: &'static str,
        /// The arguments, joined by spaces.
        args: String,
        /// The trimmed standard error output.
        stderr: String,
    },
    /// A `git` or `gh` process produced output the engine does not understand.
    #[error("unexpected output from {program} {command}: {detail}")]
    UnexpectedOutput {
        /// The program, `git` or `gh`.
        program: &'static str,
        /// The command that produced the output.
        command: String,
        /// What was wrong with it.
        detail: String,
    },
    /// An object needed for analysis is not in the repository.
    #[error("object {0} is missing from the repository")]
    MissingObject(ObjectId),
    /// A path in the repository is not valid UTF-8.
    #[error("path {0:?} is not valid UTF-8")]
    NonUtf8Path(String),
    /// The two snapshots share no history, or the shallow clone could not be deepened to find it.
    #[error("no merge base between {target} and {head}")]
    NoMergeBase {
        /// The target branch commit.
        target: ObjectId,
        /// The head commit.
        head: ObjectId,
    },
    /// A file system operation failed.
    #[error("failed to {action} {path}")]
    Io {
        /// What was being done, such as "read" or "write".
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A stored JSON file could not be decoded.
    #[error("failed to decode {path}")]
    Decode {
        /// The file.
        path: PathBuf,
        /// The decoding error.
        #[source]
        source: serde_json::Error,
    },
    /// A JSON value could not be encoded.
    #[error("failed to encode {what}")]
    Encode {
        /// What was being encoded.
        what: &'static str,
        /// The encoding error.
        #[source]
        source: serde_json::Error,
    },
    /// A complete record in the interior of the event log is corrupt.
    #[error("corrupt event at line {line} of {path}")]
    CorruptLog {
        /// The log file.
        path: PathBuf,
        /// The one-based line number of the corrupt record.
        line: usize,
        /// The decoding error.
        #[source]
        source: serde_json::Error,
    },
    /// The event log shrank below a position a reader had already passed.
    #[error("event log {path} shrank below a complete record boundary")]
    LogShrank {
        /// The log file.
        path: PathBuf,
    },
    /// The event log is inconsistent, for example with revision numbers out of sequence.
    #[error("inconsistent event log {path}: {detail}")]
    InconsistentLog {
        /// The log file.
        path: PathBuf,
        /// What is inconsistent.
        detail: String,
    },
    /// An identifier failed to parse.
    #[error(transparent)]
    ParseId(#[from] ParseIdError),
    /// No review is stored under the given key.
    #[error("no review {0}")]
    UnknownReview(String),
    /// The review has not recorded any revision yet.
    #[error("the review has no revisions yet")]
    NoRevisions,
    /// The requested revision does not exist.
    #[error("revision {0} does not exist")]
    UnknownRevision(RevisionNumber),
    /// A unit ID names a unit of a different revision than the one being addressed.
    #[error("unit {unit} belongs to revision {}, not to revision {expected}", unit.revision())]
    WrongRevision {
        /// The unit that was given.
        unit: UnitId,
        /// The revision being addressed.
        expected: RevisionNumber,
    },
    /// A verdict was given on a revision that is no longer the current one.
    #[error("revision {revision} is not current; marks go on the current revision{}", current.map(|current| format!(" ({current})")).unwrap_or_default())]
    NotCurrentRevision {
        /// The revision the mark was for.
        revision: RevisionNumber,
        /// The current revision.
        current: Option<RevisionNumber>,
    },
    /// A unit ID is out of range for its revision.
    #[error("unit {0} does not exist")]
    UnknownUnit(UnitId),
    /// A move ID is out of range or belongs to another revision.
    #[error("move {0} does not exist in this revision")]
    UnknownMove(MoveId),
    /// A move that is not a plain move was acknowledged.
    #[error("move {0} has edits; review its units instead")]
    MoveHasEdits(MoveId),
    /// A review target could not be parsed or resolved.
    #[error("invalid review target {0:?}")]
    InvalidTarget(String),
    /// No target was given, and the current branch has no open pull request, no uncommitted
    /// changes and no commits beyond the default branch.
    #[error("nothing to review: {0}; pass a pull request, a base..head range, or --worktree")]
    NothingToReview(String),
    /// A review key is already stored for a different target, so opening it would review
    /// something else under the same marks.
    #[error("review {0} is already stored for a different target")]
    KeyConflict(String),
    /// A walkthrough failed validation.
    #[error(transparent)]
    Walkthrough(#[from] WalkthroughError),
    /// The demo fixture is malformed or the requested demo revision does not exist.
    #[error("demo: {0}")]
    Demo(String),
    /// A note was empty.
    #[error("a note must not be empty")]
    EmptyNote,
    /// No unit of the revision is in the given files or directories.
    #[error("no changed file at or below {}", .0.join(", "))]
    NoChangedFiles(Vec<String>),
    /// A front end's name for its view state is not a plain lowercase name.
    #[error("invalid front-end name {0:?}; use lowercase letters, digits, `-` and `_`")]
    InvalidFrontEnd(String),
}

impl Error {
    /// Builds an [`Error::Io`] from an action, a path and the I/O error.
    pub(crate) fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }
}
