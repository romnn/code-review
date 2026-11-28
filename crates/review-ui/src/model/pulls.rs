//! The checked-out branch's pull request, looked up in the background with `gh` while a review
//! of the checkout follows it: when the review opens, when the branch or its upstream moves (a
//! switch, a rename, a push such as `gh pr create`'s), on `r`, and every couple of minutes while
//! on a branch, to see a pull request opened, retargeted, merged or closed elsewhere.
//!
//! A push that finds no pull request is looked up again 15 seconds later, since `gh pr create`
//! pushes the branch before it opens the pull request.
//! One lookup runs at a time, bounded by a timeout.
//! A lookup asks `gh` about whatever is checked out, which `gh` resolves as it does for the
//! reviewer, and answers for the branch as it was before and after `gh` ran: the branch, where
//! it pulls from and pushes to, and its upstream's commit, which together say which pull request
//! it has. One during which any of that moved is taken again, and one for another branch than
//! the window follows is kept for that branch; one naming a pull request the branch does not
//! pull fails, as `gh` resolves it the same way when asked again.
//! A failure keeps the pull request last found for the branch, shown as stale, and the next
//! lookup waits longer each time.
//! A repository where no lookup can run, without a remote, without `gh` or with `gh` not logged
//! in, is asked about again only after the longest wait, when the branch or its upstream moves,
//! or on `r`, and the overview says why meanwhile.
//!
//! A capture resolves its base from the pull request found for the branch it sees checked out,
//! of all those found so far, only while that branch pulls from and pushes to where it did
//! then, so a capture taken as the branch switches never resolves it from the branch before.
//!
//! The first lookup starts as soon as the opened checkout reaches the window, and the opening's
//! capture waits in the model until it ended for the branch the capture sees; one ended by `gh`
//! missing or failing leaves the pull request unknown, which that capture's header says.
//! Once the review shows, a lookup that can move the base captures again, and what it found is
//! said as that capture shows.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use review_core::github::{self, BranchPullRequest, CheckoutPullRequest, PullRequestState};
use review_core::{BranchIdentity, Error, Repo};

use super::effects::{Because, Outcome, Subject};
use super::pool::Lane;
use super::sync::Tick;
use super::{Effect, Model};

/// How long a lookup may take before `gh` is stopped.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How often the pull request is looked up again while on a branch.
const EVERY: Duration = Duration::from_secs(120);

/// How soon the pull request is looked up again after a push found none: `gh pr create` pushes
/// the branch before it opens the pull request.
const AFTER_PUSH: Duration = Duration::from_secs(15);

/// The longest a lookup waits after failures.
const LONGEST: Duration = Duration::from_mins(15);

/// The pull request last found for a branch.
#[derive(Debug, Clone)]
pub struct Found {
    /// The branch as it was when the pull request was found.
    pub branch: BranchIdentity,
    /// The pull request, or `None` when the branch has none.
    pub pull_request: Option<BranchPullRequest>,
    /// When it was found.
    pub at: SystemTime,
    /// Why the lookups since failed, if the last one did: it may be out of date.
    pub stale: Option<String>,
}

/// The lookups of the checked-out branch's pull request.
#[derive(Debug, Default)]
pub(crate) struct Pulls {
    /// What the lookups are for now; `None` while `HEAD` is detached or before the first
    /// capture.
    association: Option<BranchIdentity>,
    /// What the running lookup asks about.
    running: Option<BranchIdentity>,
    /// Whether another lookup follows the running one.
    again: bool,
    /// The pull request last found for each branch.
    found: HashMap<String, Found>,
    /// Failures in a row, which the wait before the next lookup doubles with.
    failures: u32,
    /// When the next periodic lookup is due.
    due: Option<Instant>,
    /// Why no lookup can run, such as a repository without a remote.
    unavailable: Option<String>,
    /// Why the last lookup failed, if it did.
    failed: Option<String>,
    /// Whether the branch's upstream just moved, so a lookup that finds no pull request is
    /// followed soon by another.
    pushed: bool,
    /// Whether a lookup for the association ended, with an answer or a failure, which the
    /// opening's capture waits for.
    settled: bool,
    /// Whether `gh` could not answer for the association: it is missing, not logged in, failed
    /// or timed out, so the branch may have a pull request nothing knows of.
    unanswered: bool,
}

impl Pulls {
    /// The pull request last found for the branch checked out, if one was while it pulled from
    /// and pushed to where it does now.
    pub(crate) fn current(&self) -> Option<&Found> {
        let association = self.association.as_ref()?;
        self.found
            .get(&association.branch)
            .filter(|found| found.branch.tracks_the_same(association))
    }

    /// What the captures resolve their base from: what was last found for each branch, of
    /// which a capture takes what was found for the branch it sees checked out.
    pub(crate) fn pull_requests(&self) -> Vec<CheckoutPullRequest> {
        self.found
            .values()
            .map(|found| CheckoutPullRequest {
                branch: found.branch.clone(),
                pull_request: found.pull_request.clone(),
            })
            .collect()
    }

    /// Why the pull request is not looked up, if it is not.
    pub(crate) fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// Whether a lookup runs.
    pub(crate) fn running(&self) -> bool {
        self.running.is_some()
    }

    /// Why the last lookup failed, if it did.
    pub(crate) fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// Whether the opening's capture may be taken: the association's lookup ended, or there is
    /// nothing to look up, as while `HEAD` is detached.
    pub(super) fn settled(&self) -> bool {
        self.association.is_none() || self.settled
    }

    /// Whether the branch's pull request is unknown because `gh` could not answer for it and
    /// nothing was found for it before: a capture then compares with the default branch and
    /// says so.
    pub(super) fn unknown(&self) -> bool {
        self.unanswered && self.current().is_none()
    }
}

impl Pulls {
    /// Keeps `found` as what was last found for its branch, returning what was before.
    fn keep(&mut self, found: CheckoutPullRequest) -> Option<Found> {
        self.found.insert(
            found.branch.branch.clone(),
            Found {
                branch: found.branch,
                pull_request: found.pull_request,
                at: SystemTime::now(),
                stale: None,
            },
        )
    }
}

/// What a lookup found, with the branch it asked about.
pub(super) struct Looked {
    asked: BranchIdentity,
    result: Result<CheckoutPullRequest, Failure>,
}

/// Why a lookup found nothing.
#[derive(Debug)]
enum Failure {
    /// No lookup can run in this repository, for this reason; `gh` says whether it is `gh`
    /// that cannot run, so the branch may have a pull request it cannot see.
    Unavailable { why: String, gh: bool },
    /// This lookup failed, for this reason.
    Failed(String),
    /// The branch, where it pulls from or pushes to, or its upstream's commit moved while `gh`
    /// ran, so its answer may be for another.
    Moved,
    /// `HEAD` was detached when `gh` ran.
    Detached,
}

impl Model {
    /// Takes in what the shown capture's branch and upstream are: when they changed, looks up
    /// the pull request at once; while `HEAD` is detached, nothing is looked up.
    pub(super) fn associate(&mut self, association: Option<BranchIdentity>) {
        let pulls = &mut self.pulls;
        if pulls.association == association {
            return;
        }
        let branch = |association: &Option<BranchIdentity>| {
            association
                .as_ref()
                .map(|association| association.branch.clone())
        };
        pulls.pushed = branch(&pulls.association) == branch(&association);
        pulls.association = association;
        pulls.failures = 0;
        pulls.due = None;
        pulls.settled = false;
        pulls.unanswered = false;
        // Another branch or upstream may have what this one lacked, such as a remote.
        pulls.unavailable = None;
        self.look_up_pull_request();
    }

    /// Looks up the checked-out branch's pull request in the background, or after the running
    /// lookup.
    pub(super) fn look_up_pull_request(&mut self) {
        if self.engine.is_none() {
            return;
        }
        if self.replaced.is_some() {
            return;
        }
        let pulls = &mut self.pulls;
        let Some(association) = pulls.association.clone() else {
            return;
        };
        // Where no lookup can run, the next waits for its time.
        if pulls.unavailable.is_some() && pulls.due.is_none_or(|due| due > Instant::now()) {
            return;
        }
        if pulls.running.is_some() {
            pulls.again = true;
            return;
        }
        pulls.running = Some(association.clone());
        // It mostly waits for `gh`, and one runs at a time.
        self.spawn_with(Lane::Own, Subject::Always(Because::Live), move |parts| {
            let result = find(parts.review.repo());
            Outcome::PullRequest(Box::new(Looked {
                asked: association,
                result,
            }))
        });
    }

    /// `r` on a review that follows the checkout: looks up the pull request now, even where
    /// it could not be before, such as after `gh` was installed.
    pub(super) fn look_up_pull_request_now(&mut self) {
        self.pulls.unavailable = None;
        self.pulls.failures = 0;
        self.look_up_pull_request();
    }

    /// Takes in a lookup: keeps what it found for its branch, and captures again when the
    /// change can move the base, whose capture says what changed as it shows; takes it again
    /// when the branch moved while `gh` ran, and keeps an answer for another branch than the
    /// window follows for that one.
    ///
    /// While the review opens, a lookup that ended for the association lets the opening's
    /// capture go ahead, and one that found the checkout on another branch, or detached, looks
    /// that up. A review that ended in the window, or a window that quits, takes none in.
    pub(super) fn looked_up(&mut self, looked: Looked) {
        self.pulls.running = None;
        if self.replaced.is_some() || self.quitting.over() {
            return;
        }
        let opening = self.checking.is_some();
        let pulls = &mut self.pulls;
        let again = std::mem::take(&mut pulls.again);
        let Looked { asked, result } = looked;
        let current = pulls.association.as_ref() == Some(&asked);
        match result {
            Ok(found) if pulls.association.as_ref() != Some(&found.branch) => {
                // An answer for the branch as it is on disk, ahead of the capture that shows
                // it, or one the window moved on from: it holds for that branch, and the
                // window's own branch is looked up once its capture shows, or when due.
                let branch = found.branch.clone();
                pulls.keep(found);
                if opening {
                    self.associate(Some(branch));
                    self.confirm_opening();
                    return;
                }
                self.recapture_for_pull_requests();
                self.look_up_later(EVERY, again);
                return;
            }
            Ok(found) => {
                pulls.failures = 0;
                pulls.failed = None;
                pulls.unavailable = None;
                pulls.settled = true;
                pulls.unanswered = false;
                pulls.keep(found);
            }
            Err(Failure::Moved) => {
                self.look_up_pull_request();
                return;
            }
            Err(Failure::Detached) if opening => {
                self.associate(None);
                self.confirm_opening();
                return;
            }
            Err(Failure::Detached) => {
                self.look_up_later(EVERY, again);
                return;
            }
            Err(Failure::Unavailable { why, gh }) => {
                if current {
                    pulls.settled = true;
                    pulls.unanswered = gh;
                }
                // Asked again after the longest wait, as after `gh auth login` or a remote
                // added meanwhile.
                if pulls.unavailable.replace(why.clone()).as_ref() != Some(&why) {
                    self.effect(Effect::Log(format!(
                        "the branch's pull request is not looked up: {why}"
                    )));
                }
                self.look_up_later(LONGEST, false);
                self.after_lookup();
                return;
            }
            Err(Failure::Failed(why)) => {
                if current {
                    pulls.settled = true;
                    pulls.unanswered = true;
                }
                let branch = asked.branch;
                pulls.failures = pulls.failures.saturating_add(1);
                pulls.failed = Some(first_line(&why));
                if let Some(found) = pulls.found.get_mut(&branch) {
                    found.stale = Some(first_line(&why));
                }
                let wait = backoff(pulls.failures);
                self.effect(Effect::Log(format!(
                    "could not look up the pull request of {branch}: {why}"
                )));
                self.look_up_later(wait, again);
                self.after_lookup();
                return;
            }
        }
        let pushed = std::mem::take(&mut pulls.pushed);
        let none = pulls
            .current()
            .is_some_and(|found| found.pull_request.is_none());
        let wait = if pushed && none {
            AFTER_PUSH
        } else {
            backoff(pulls.failures)
        };
        self.look_up_later(wait, again);
        self.after_lookup();
    }

    /// What follows a lookup that ended: while the review opens, its capture once the history
    /// is replayed, taken anew when what was found changed since one was asked for; once it
    /// shows, a capture again when what was found can move the base or what the header says
    /// of it.
    fn after_lookup(&mut self) {
        if self.checking.is_some() {
            self.confirm_opening();
        } else {
            self.recapture_for_pull_requests();
        }
    }

    /// Captures again when the pull requests found differ from those the capture on screen, the
    /// one waiting to show or the running one resolved its base with, or when whether the pull
    /// request is unknown changed: the base may follow them, so what shows waits for a capture
    /// that knows them.
    fn recapture_for_pull_requests(&mut self) {
        let Some(live) = &self.live else {
            return;
        };
        let found = self.pulls.pull_requests();
        let running = live.given().is_some_and(|given| !same_found(given, &found));
        let outdated = |subject: &review_core::Subject| {
            subject.branch.as_ref().is_some_and(|branch| {
                let cached = self
                    .pulls
                    .found
                    .get(branch)
                    .and_then(|found| found.pull_request.as_ref());
                cached != subject.pull_request.as_ref()
            })
        };
        let shown = live.subject().is_some_and(outdated) || live.unknown() != self.pulls.unknown();
        let waiting = live.waiting_subject().is_some_and(outdated);
        if running || shown || waiting {
            self.recapture_for_subject();
        }
    }

    /// Looks the pull request up again after `wait`, and at once too when `again` asks for it.
    fn look_up_later(&mut self, wait: Duration, again: bool) {
        self.pulls.due = Some(Instant::now() + wait);
        self.after(
            wait,
            Subject::Always(Because::Live),
            Outcome::Tick(Tick::PullRequest),
        );
        if again {
            self.look_up_pull_request();
        }
    }

    /// The periodic lookup's time: looks up the pull request once it is due.
    pub(super) fn pull_request_due(&mut self) {
        let due = self.pulls.due.is_some_and(|due| due <= Instant::now());
        if due {
            self.look_up_pull_request();
        }
    }
}

/// The first line of `text`, as a lookup's failure shows in a line of its own; `gh` and the
/// tools around it may say more on the lines after.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// How long the next lookup waits after `failures` failures in a row: [`EVERY`], doubling with
/// each failure up to [`LONGEST`].
fn backoff(failures: u32) -> Duration {
    EVERY
        .saturating_mul(2_u32.saturating_pow(failures.min(8)))
        .min(LONGEST)
}

/// Whether `given` and `found` hold the same pull requests found for branches, in any order.
fn same_found(given: &[CheckoutPullRequest], found: &[CheckoutPullRequest]) -> bool {
    given.len() == found.len() && given.iter().all(|given| found.contains(given))
}

/// What the confirmed capture of the opening was given of the lookups: what was found for
/// each branch, whether `gh` could not answer for the branch looked up, and that branch.
#[derive(Clone)]
pub(super) struct Given {
    pull_requests: Vec<CheckoutPullRequest>,
    unknown: bool,
    asked: Option<BranchIdentity>,
}

impl Given {
    /// What the capture is given of `pulls`.
    pub(super) fn of(pulls: &Pulls) -> Self {
        Self {
            pull_requests: pulls.pull_requests(),
            unknown: pulls.unknown(),
            asked: pulls.association.clone(),
        }
    }

    /// What was found for each branch.
    pub(super) fn pull_requests(&self) -> &[CheckoutPullRequest] {
        &self.pull_requests
    }

    /// Whether `gh` could not answer for the branch looked up.
    pub(super) fn unknown(&self) -> bool {
        self.unknown
    }

    /// The branch looked up.
    pub(super) fn asked(&self) -> Option<&BranchIdentity> {
        self.asked.as_ref()
    }
}

impl PartialEq for Given {
    fn eq(&self, other: &Self) -> bool {
        same_found(&self.pull_requests, &other.pull_requests)
            && self.unknown == other.unknown
            && self.asked == other.asked
    }
}

/// What changed about `branch`'s pull request, from `before` to `now`, as the status line says
/// it; `None` when nothing that shows did.
pub(super) fn news(
    branch: &str,
    before: Option<&BranchPullRequest>,
    now: Option<&BranchPullRequest>,
) -> Option<String> {
    let state = |pull_request: &BranchPullRequest| match pull_request.state {
        PullRequestState::Open => "open",
        PullRequestState::Merged => "merged",
        PullRequestState::Closed => "closed",
    };
    let Some(now) = now else {
        return before.map(|before| {
            format!(
                "Pull request #{} no longer belongs to {branch}.",
                before.number
            )
        });
    };
    let Some(before) = before else {
        return Some(format!(
            "{branch} has pull request #{} ({}).",
            now.number,
            state(now)
        ));
    };
    if before.number != now.number {
        Some(format!("Pull request #{} is {branch}'s now.", now.number))
    } else if before.state != now.state {
        Some(match now.state {
            PullRequestState::Open => format!("#{} reopened.", now.number),
            PullRequestState::Merged => format!("#{} merged.", now.number),
            PullRequestState::Closed => format!("#{} closed.", now.number),
        })
    } else if before.base_branch != now.base_branch || before.base_repository != now.base_repository
    {
        Some(format!(
            "#{} now merges into {}.",
            now.number, now.base_branch
        ))
    } else {
        None
    }
}

/// Looks up the pull request of the branch checked out in `repo`; runs in the background.
fn find(repo: &Repo) -> Result<CheckoutPullRequest, Failure> {
    match repo.default_remote() {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(Failure::Unavailable {
                why: "the repository has no remote".to_owned(),
                gh: false,
            });
        }
        Err(err) => return Err(Failure::Failed(err.to_string())),
    }
    match github::for_checkout(repo, TIMEOUT) {
        Ok(Some(found)) => Ok(found),
        Ok(None) => Err(Failure::Detached),
        Err(Error::Raced(_)) => Err(Failure::Moved),
        Err(Error::Spawn { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Err(Failure::Unavailable {
                why: "gh, the GitHub CLI, is not installed".to_owned(),
                gh: true,
            })
        }
        Err(Error::CommandFailed { stderr, .. }) => {
            let lower = stderr.to_lowercase();
            if lower.contains("known github host") || lower.contains("no git remotes") {
                Err(Failure::Unavailable {
                    why: "no remote of the repository is on GitHub".to_owned(),
                    gh: false,
                })
            } else if lower.contains("gh auth login") {
                Err(Failure::Unavailable {
                    why: "gh is not logged in; run gh auth login".to_owned(),
                    gh: true,
                })
            } else {
                Err(Failure::Failed(stderr))
            }
        }
        Err(err) => Err(Failure::Failed(err.to_string())),
    }
}

#[cfg(test)]
impl Looked {
    /// A lookup for `branch` that found `pull_request`.
    pub(super) fn found(branch: BranchIdentity, pull_request: Option<BranchPullRequest>) -> Self {
        Self {
            asked: branch.clone(),
            result: Ok(CheckoutPullRequest {
                branch,
                pull_request,
            }),
        }
    }

    /// A lookup for `branch` that failed.
    pub(super) fn failed(branch: BranchIdentity) -> Self {
        Self {
            asked: branch,
            result: Err(Failure::Failed("gh timed out".to_owned())),
        }
    }

    /// A lookup for `branch` that found no lookup can run.
    pub(super) fn unavailable(branch: BranchIdentity) -> Self {
        Self {
            asked: branch,
            result: Err(Failure::Unavailable {
                why: "gh is not logged in; run gh auth login".to_owned(),
                gh: true,
            }),
        }
    }
}

#[cfg(test)]
impl Pulls {
    /// What the lookups are for now.
    pub(super) fn association(&self) -> Option<&BranchIdentity> {
        self.association.as_ref()
    }

    /// Makes the next lookup due now, as if its wait passed.
    pub(super) fn make_due(&mut self) {
        self.due = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wait before the next lookup doubles with each failure, up to a quarter of an hour.
    #[test]
    fn lookups_back_off_after_failures() {
        assert_eq!(backoff(0), EVERY);
        assert_eq!(backoff(1), EVERY * 2);
        assert_eq!(backoff(2), EVERY * 4);
        assert_eq!(backoff(20), LONGEST);
    }
}
