//! Choosing a review when none is given: the remembered one, the branch's pull request, or the
//! default branch against the current branch.
//!
//! These repositories have no GitHub remote, so `gh` fails or is missing whatever the machine has
//! installed, and the pull request rule always falls through.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{Detected, Error, Repo, Review, ReviewTarget};

use crate::common::TestRepo;

/// A repository with `base` committed on `main` and one more commit on `feature`.
fn feature_repo() -> eyre::Result<TestRepo> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn a() {}\n")?;
    test.commit("base")?;
    test.branch_at("feature", "main")?;
    test.write("lib.rs", "fn a() {}\nfn b() {}\n")?;
    test.commit("feature")?;
    Ok(test)
}

/// The default branch is what `origin/HEAD` names, then `origin/main`, `origin/master`, `main`
/// and `master`, whichever exists first.
#[gtest]
fn default_branch_follows_origin_head_then_fixed_names() -> eyre::Result<()> {
    // Only a local `master`.
    let test = TestRepo::new()?;
    test.commit("base")?;
    test.git(&["branch", "--move", "main", "master"])?;
    let repo = Repo::discover(test.path())?;
    expect_that!(repo.default_branch()?, some(eq("master")));

    // A local `main` wins over `master`.
    test.git(&["branch", "main"])?;
    expect_that!(repo.default_branch()?, some(eq("main")));

    // A remote-tracking `origin/main` wins over local branches.
    let upstream = TestRepo::new()?;
    upstream.commit("upstream base")?;
    upstream.git(&["branch", "trunk"])?;
    let upstream_path = upstream.path().display().to_string();
    test.git(&["remote", "add", "origin", &upstream_path])?;
    test.git(&["fetch", "--quiet", "origin"])?;
    expect_that!(repo.default_branch()?, some(eq("origin/main")));

    // `origin/HEAD` wins over every fixed name, even when it names an unusual branch.
    test.git(&["remote", "set-head", "origin", "trunk"])?;
    expect_that!(repo.default_branch()?, some(eq("origin/trunk")));

    // A dangling `origin/HEAD` falls back to the fixed names.
    test.git(&["update-ref", "-d", "refs/remotes/origin/trunk"])?;
    expect_that!(repo.default_branch()?, some(eq("origin/main")));
    Ok(())
}

/// A repository without any of the candidate branches has no default branch.
#[gtest]
fn no_default_branch_without_candidates() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.commit("base")?;
    test.git(&["branch", "--move", "main", "trunk"])?;
    let repo = Repo::discover(test.path())?;
    expect_that!(repo.default_branch()?, none());
    Ok(())
}

/// Without a usable pull request, the target is the default branch against the current branch,
/// and the detection says why the pull request was skipped.
#[gtest]
fn falls_back_to_the_default_branch_range_when_gh_fails() -> eyre::Result<()> {
    let test = feature_repo()?;
    let repo = Repo::discover(test.path())?;
    let (target, detected) = ReviewTarget::for_current_branch(&repo)?;
    expect_that!(
        target,
        eq(&ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned(),
        })
    );
    let Detected::Range {
        base,
        head,
        no_pull_request,
    } = detected
    else {
        eyre::bail!("expected a range, got {detected:?}");
    };
    expect_that!(base, eq("main"));
    expect_that!(head, eq("feature"));
    // Either gh is missing or it finds no remote; both name gh.
    expect_that!(no_pull_request, contains_substring("gh"));

    // The range key is the one an explicit `main..feature` gets, so marks are shared.
    let explicit = ReviewTarget::parse("main..feature", &repo)?;
    expect_that!(target.key(&repo)?, eq(&explicit.key(&repo)?));
    Ok(())
}

/// A detached `HEAD` with commits beyond the default branch is reviewed as `<default>..HEAD`.
#[gtest]
fn detached_head_reviews_up_to_head() -> eyre::Result<()> {
    let test = feature_repo()?;
    test.git(&["checkout", "--quiet", "--detach", "feature"])?;
    let repo = Repo::discover(test.path())?;
    let (target, _) = ReviewTarget::for_current_branch(&repo)?;
    expect_that!(
        target,
        eq(&ReviewTarget::Range {
            base: "main".to_owned(),
            head: "HEAD".to_owned(),
        })
    );
    Ok(())
}

/// On the default branch itself, or a detached `HEAD` at it, there is nothing to compare.
#[gtest]
fn nothing_to_review_on_the_default_branch() -> eyre::Result<()> {
    let test = feature_repo()?;
    let repo = Repo::discover(test.path())?;
    for checkout in [
        &["checkout", "--quiet", "main"][..],
        &["checkout", "--quiet", "--detach"],
    ] {
        test.git(checkout)?;
        let result = ReviewTarget::for_current_branch(&repo);
        let Err(Error::NothingToReview(reason)) = result else {
            eyre::bail!("expected nothing to review, got {result:?}");
        };
        expect_that!(reason, contains_substring("no commits beyond main"));
    }
    Ok(())
}

/// Once a review is opened on a branch, opening without a target returns to it.
#[gtest]
fn the_remembered_review_wins() -> eyre::Result<()> {
    let test = feature_repo()?;
    let repo = Repo::discover(test.path())?;
    let (first, detected) = Review::open_for_current_branch(test.data_dir(), repo.clone())?;
    expect_that!(detected, matches_pattern!(Detected::Range { .. }));
    review_core::store::remember_review(test.data_dir(), first.repo(), first.key())?;

    let (again, detected) = Review::open_for_current_branch(test.data_dir(), repo)?;
    expect_that!(detected, eq(&Detected::Remembered));
    expect_that!(again.key(), eq(first.key()));
    Ok(())
}
