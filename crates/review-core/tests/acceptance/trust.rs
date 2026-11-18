//! Trust: whether tools that can execute reviewed code, such as language servers, may run.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{Error, ObjectId, Repo, Review, ReviewTarget, Trust, TrustScope};

use crate::common::{TestRepo, refresh};

/// Opens a review of `target` in `repo`'s data directory.
fn open(repo: &TestRepo, target: ReviewTarget) -> eyre::Result<Review> {
    Ok(Review::open(
        repo.data_dir(),
        Repo::discover(repo.path())?,
        target,
    )?)
}

/// No review is trusted until the reviewer decides, whoever wrote the code, and a decision is
/// stored in the log, read back by a fresh handle, and overridden by the next decision.
#[test]
fn trust_defaults_and_decisions_round_trip() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", &base)?;
    repo.write("a.txt", "two\n")?;
    let head = repo.commit("head")?;

    // The working tree and a local branch: what they hold may come from anywhere.
    let worktree = open(
        &repo,
        ReviewTarget::Worktree {
            base: "main".to_owned(),
        },
    )?;
    assert_that!(
        worktree.trust()?,
        eq(Trust {
            trusted: false,
            decided: false
        })
    );
    let local = repo.review()?;
    refresh(&local)?;
    assert_that!(
        local.trust()?,
        eq(Trust {
            trusted: false,
            decided: false
        })
    );

    // Nor are a pull request and a bare commit.
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
    pull_request.set_trusted(true)?;
    let reopened = Review::load(repo.data_dir(), pull_request.key())?;
    assert_that!(
        reopened.trust()?,
        eq(Trust {
            trusted: true,
            decided: true
        })
    );
    assert_that!(reopened.history()?.revisions().len(), eq(0));

    // The latest decision wins.
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

/// A review of a range or of the working tree, whose key another clone at the same path
/// shares, is trusted one snapshot at a time: a grant for the whole review is refused, and one
/// for a snapshot covers that snapshot alone.
#[test]
fn a_range_is_trusted_one_snapshot_at_a_time() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", &base)?;
    repo.write("a.txt", "two\n")?;
    repo.commit("head")?;
    let local = repo.review()?;
    refresh(&local)?;
    assert_that!(
        local.set_trusted(true),
        err(matches_pattern!(Error::InvalidTarget(_)))
    );

    let tree = ObjectId::parse(&repo.git(&["rev-parse", "feature^{tree}"])?)?;
    let other = ObjectId::parse(&repo.git(&["rev-parse", "main^{tree}"])?)?;
    local.set_trust(TrustScope::Snapshot { tree: tree.clone() }, true)?;
    assert_that!(local.trust_for(&tree)?.trusted, eq(true));
    assert_that!(local.trust_for(&other)?.trusted, eq(false));
    assert_that!(local.trust()?.trusted, eq(false));
    Ok(())
}
