//! Trust: whether tools that run the code in a checkout, such as language servers, may run.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{Error, Repo, Review, ReviewTarget};

use crate::common::{TestRepo, refresh};

/// Opens a review of `target` in `repo`'s data directory.
fn open(repo: &TestRepo, target: ReviewTarget) -> eyre::Result<Review> {
    Ok(Review::open(
        repo.data_dir(),
        Repo::discover(repo.path())?,
        target,
    )?)
}

/// Only the review of a checkout holds trust: a review of a range or a pull request has no code
/// on disk and is never trusted, while a review of the working tree against a fixed ref shares
/// the checkout's grant.
#[test]
fn only_the_checkout_holds_trust() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", &base)?;
    repo.write("a.txt", "two\n")?;
    repo.commit("head")?;

    // A range and a pull request: nothing to trust, and nothing that holds trust for them.
    let range = repo.review()?;
    refresh(&range)?;
    let pull_request = open(
        &repo,
        ReviewTarget::PullRequest {
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            repo: "repo".to_owned(),
            number: 7,
        },
    )?;
    for review in [&range, &pull_request] {
        assert_that!(review.trusted()?, eq(false));
        assert_that!(
            review.set_trusted(true),
            err(matches_pattern!(Error::InvalidTarget(_)))
        );
        assert!(review.checkout_review()?.is_none());
    }

    // The working tree against a fixed ref: the checkout's review holds its trust.
    let worktree = open(
        &repo,
        ReviewTarget::Worktree {
            base: "main".to_owned(),
        },
    )?;
    let checkout = Review::open_checkout(repo.data_dir(), Repo::discover(repo.path())?)?;
    let holder = worktree
        .checkout_review()?
        .ok_or_else(|| eyre::eyre!("no review holds the trust"))?;
    assert_that!(holder.key(), eq(checkout.key()));
    assert_that!(worktree.trusted()?, eq(false));

    // A grant survives reopening the review, and the latest decision wins.
    assert_that!(checkout.trusted()?, eq(false));
    checkout.set_trusted(true)?;
    assert_that!(holder.trusted()?, eq(true));
    let reopened = Review::load(repo.data_dir(), checkout.key())?;
    assert_that!(reopened.trusted()?, eq(true));
    assert_that!(reopened.history()?.revisions().len(), eq(0));
    checkout.set_trusted(false)?;
    assert_that!(holder.trusted()?, eq(false));
    Ok(())
}
