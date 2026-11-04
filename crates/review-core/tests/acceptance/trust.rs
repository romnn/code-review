//! Trust: whether tools that can execute reviewed code, such as language servers, may run.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{Repo, Review, ReviewTarget, Trust};

use crate::common::{TestRepo, refresh};

/// Opens a review of `target` in `repo`'s data directory.
fn open(repo: &TestRepo, target: ReviewTarget) -> eyre::Result<Review> {
    Ok(Review::open(
        repo.data_dir(),
        Repo::discover(repo.path())?,
        target,
    )?)
}

/// Defaults follow the provenance of the reviewed code, and a decision is stored in the log, read
/// back by a fresh handle, and overridden by the next decision.
#[test]
fn trust_defaults_and_decisions_round_trip() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", &base)?;
    repo.write("a.txt", "two\n")?;
    let head = repo.commit("head")?;

    // Own code: the working tree and a local branch are trusted until decided otherwise.
    let worktree = open(
        &repo,
        ReviewTarget::Worktree {
            base: "main".to_owned(),
        },
    )?;
    assert_that!(
        worktree.trust()?,
        eq(Trust {
            trusted: true,
            decided: false
        })
    );
    let local = repo.review()?;
    refresh(&local)?;
    assert_that!(
        local.trust()?,
        eq(Trust {
            trusted: true,
            decided: false
        })
    );

    // Someone else's code: a pull request and a bare commit are not.
    let pull_request = open(
        &repo,
        ReviewTarget::PullRequest {
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            repo: "repo".to_owned(),
            number: 7,
        },
    )?;
    assert_that!(
        pull_request.trust()?,
        eq(Trust {
            trusted: false,
            decided: false
        })
    );
    let commit = open(
        &repo,
        ReviewTarget::Range {
            base: "main".to_owned(),
            head,
        },
    )?;
    assert_that!(
        commit.trust()?,
        eq(Trust {
            trusted: false,
            decided: false
        })
    );

    // A decision survives reopening the review and does not disturb its history.
    local.set_trusted(false)?;
    let reopened = Review::load(repo.data_dir(), local.key())?;
    assert_that!(
        reopened.trust()?,
        eq(Trust {
            trusted: false,
            decided: true
        })
    );
    assert_that!(reopened.history()?.revisions().len(), eq(1));

    // The latest decision wins.
    pull_request.set_trusted(true)?;
    pull_request.set_trusted(false)?;
    pull_request.set_trusted(true)?;
    assert_that!(
        pull_request.trust()?,
        eq(Trust {
            trusted: true,
            decided: true
        })
    );
    Ok(())
}
