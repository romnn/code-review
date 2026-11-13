//! Choosing a review when none is given: the remembered one, the branch's pull request, the
//! working tree against the default branch, or the default branch against the current branch.
//!
//! These repositories have no GitHub remote, so `gh` fails or is missing whatever the machine has
//! installed, and the pull request rule always falls through.

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::indoc;
use review_core::{Detected, Error, MarkVerdict, Repo, Review, ReviewTarget};

use crate::common::{TestRepo, mark_all, refresh};

/// A repository with `base` committed on `main` and one more commit on `feature`.
fn feature_repo() -> eyre::Result<TestRepo> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn a() {}\n")?;
    test.commit("base")?;
    test.branch_at("feature", "main")?;
    test.write(
        "lib.rs",
        indoc! {"
            fn a() {}
            fn b() {}
        "},
    )?;
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

/// Uncommitted work on a branch with no commits of its own is reviewed as the working tree
/// against the default branch, under the key an explicit `--worktree main` gets.
#[gtest]
fn uncommitted_work_reviews_the_working_tree() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn a() {}\n")?;
    test.commit("base")?;
    test.git(&["checkout", "--quiet", "-b", "feature"])?;
    test.write("lib.rs", "fn b() {}\n")?;
    let repo = Repo::discover(test.path())?;

    let (target, detected) = ReviewTarget::for_current_branch(&repo)?;
    expect_that!(
        target,
        eq(&ReviewTarget::Worktree {
            base: "main".to_owned(),
        })
    );
    expect_that!(
        detected,
        matches_pattern!(Detected::Worktree {
            base: eq("main"),
            no_pull_request: contains_substring("gh"),
        })
    );
    let explicit = ReviewTarget::from_arguments(None, Some("main"), &repo)?;
    expect_that!(target.key(&repo)?, eq(&explicit.key(&repo)?));
    Ok(())
}

/// Work begun on the default branch and carried to a new branch with `git checkout -b` stays
/// one review: marks made on the default branch still hold on the new branch, and the new branch
/// keeps the review once its work is committed and the tree is clean again.
#[gtest]
fn working_tree_review_follows_work_to_a_new_branch() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn a() {}\n")?;
    test.commit("base")?;
    let repo = Repo::discover(test.path())?;
    let data = test.data_dir();

    // Open and accept uncommitted work on the default branch
    test.write("lib.rs", "fn b() {}\n")?;
    let (on_main, detected) = Review::open_for_current_branch(data, repo.clone())?;
    expect_that!(detected, matches_pattern!(Detected::Worktree { .. }));
    let first = refresh(&on_main)?;
    mark_all(&on_main, first, MarkVerdict::Accepted)?;
    review_core::store::remember_review(data, on_main.repo(), on_main.key())?;

    // The same work on a new branch opens the same review, with its marks
    test.git(&["checkout", "--quiet", "-b", "feature"])?;
    let (on_feature, detected) = Review::open_for_current_branch(data, repo.clone())?;
    expect_that!(detected, matches_pattern!(Detected::Worktree { .. }));
    expect_that!(on_feature.key(), eq(on_main.key()));
    review_core::store::remember_review(data, on_feature.repo(), on_feature.key())?;

    // Committed in full, the clean branch returns to the remembered review, whose content is
    // unchanged, so the accepted unit is still accepted.
    test.commit("feature")?;
    let (committed, detected) = Review::open_for_current_branch(data, repo)?;
    expect_that!(detected, eq(&Detected::Remembered));
    expect_that!(committed.key(), eq(on_main.key()));
    let current = refresh(&committed)?;
    let history = committed.history()?;
    let total = history.view(current)?.status().total;
    expect_that!((total.units, total.accepted), eq((1, 1)));
    Ok(())
}

/// A branch with commits and more uncommitted work reviews the working tree, so the uncommitted
/// part is never left out.
#[gtest]
fn uncommitted_work_beyond_commits_reviews_the_working_tree() -> eyre::Result<()> {
    let test = feature_repo()?;
    test.write("lib.rs", "fn c() {}\n")?;
    let repo = Repo::discover(test.path())?;
    let (target, _) = ReviewTarget::for_current_branch(&repo)?;
    expect_that!(
        target,
        eq(&ReviewTarget::Worktree {
            base: "main".to_owned(),
        })
    );
    Ok(())
}

/// A bare repository has no working tree to review, so a branch with commits beyond the default
/// branch is reviewed as a range.
#[gtest]
fn bare_repository_reviews_the_range() -> eyre::Result<()> {
    let test = feature_repo()?;
    let bare = tempfile::tempdir()?;
    let bare_path = bare.path().join("repo.git");
    let source = test.path().display().to_string();
    let destination = bare_path.display().to_string();
    test.git(&["clone", "--quiet", "--bare", &source, &destination])?;
    let repo = Repo::discover(&bare_path)?;
    let (target, _) = ReviewTarget::for_current_branch(&repo)?;
    expect_that!(
        target,
        eq(&ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned(),
        })
    );
    Ok(())
}

/// `gh` looks for a bare repository's pull request in that repository, not in whatever checkout
/// the process runs in.
#[cfg(unix)]
#[gtest]
fn gh_runs_in_a_bare_repository() -> eyre::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let test = feature_repo()?;
    let scratch = tempfile::tempdir()?;
    let bare_path = scratch.path().join("repo.git");
    let source = test.path().display().to_string();
    let destination = bare_path.display().to_string();
    test.git(&["clone", "--quiet", "--bare", &source, &destination])?;

    // A `gh` that records where it ran and finds no pull request.
    let bin = scratch.path().join("bin");
    std::fs::create_dir(&bin)?;
    let gh = bin.join("gh");
    std::fs::write(
        &gh,
        indoc! {r#"
            #!/bin/sh
            pwd -P > "$GH_CWD_FILE"
            exit 1
        "#},
    )?;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
    let cwd_file = scratch.path().join("gh-cwd");
    let mut path = bin.into_os_string();
    if let Some(inherited) = std::env::var_os("PATH") {
        path.push(":");
        path.push(inherited);
    }

    let mut repo = Repo::discover(&bare_path)?;
    repo.set_command_env([
        ("PATH", path),
        ("GH_CWD_FILE", cwd_file.clone().into_os_string()),
    ]);
    ReviewTarget::for_current_branch(&repo)?;
    let ran_in = std::fs::read_to_string(&cwd_file)?;
    expect_that!(
        std::path::Path::new(ran_in.trim()),
        eq(bare_path.canonicalize()?.as_path())
    );
    Ok(())
}

/// An untracked file is uncommitted work even when `status.showUntrackedFiles` hides it, since a
/// working-tree capture adds it.
#[gtest]
fn untracked_files_count_whatever_status_shows() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", "fn a() {}\n")?;
    test.commit("base")?;
    test.git(&["checkout", "--quiet", "-b", "feature"])?;
    test.git(&["config", "status.showUntrackedFiles", "no"])?;
    test.write("new.rs", "fn new() {}\n")?;
    let repo = Repo::discover(test.path())?;
    expect_that!(repo.has_uncommitted_changes()?, eq(true));

    // An ignored file is not work to review.
    test.git(&["config", "status.showUntrackedFiles", "normal"])?;
    test.write(".git/info/exclude", "new.rs\n")?;
    expect_that!(repo.has_uncommitted_changes()?, eq(false));
    Ok(())
}

/// A detached `HEAD` is never reviewed as the working tree, because that review is trusted and a
/// detached commit may be someone else's code.
#[gtest]
fn detached_head_never_reviews_the_working_tree() -> eyre::Result<()> {
    let test = feature_repo()?;
    test.git(&["checkout", "--quiet", "--detach", "feature"])?;
    test.write("lib.rs", "fn dirty() {}\n")?;
    let repo = Repo::discover(test.path())?;
    let (target, _) = ReviewTarget::for_current_branch(&repo)?;
    expect_that!(
        target,
        eq(&ReviewTarget::Range {
            base: "main".to_owned(),
            head: "HEAD".to_owned(),
        })
    );

    // Without commits beyond the default branch there is nothing to review.
    test.git(&["checkout", "--quiet", "lib.rs"])?;
    test.git(&["checkout", "--quiet", "--detach", "main"])?;
    test.write("untracked.rs", "fn dirty() {}\n")?;
    let result = ReviewTarget::for_current_branch(&repo);
    let Err(Error::NothingToReview(_)) = result else {
        eyre::bail!("expected nothing to review, got {result:?}");
    };
    Ok(())
}

/// Changes inside a submodule's own checkout are not uncommitted work of the superproject: a
/// capture records only the submodule's commit, so such a review would show nothing.
#[gtest]
fn dirty_submodule_contents_are_not_uncommitted_work() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("sub/inner.rs", "fn inner() {}\n")?;
    let sub = test.path().join("sub");
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["add", "--all"],
        &["commit", "--quiet", "--no-verify", "-m", "inner"],
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&sub)
            .args(args)
            .envs(crate::common::GIT_ENV)
            .status()?;
        eyre::ensure!(status.success(), "git {} failed in sub", args.join(" "));
    }
    test.commit("base with an embedded repository")?;
    test.git(&["checkout", "--quiet", "-b", "feature"])?;
    test.write("sub/inner.rs", "fn changed() {}\n")?;
    let repo = Repo::discover(test.path())?;
    expect_that!(repo.has_uncommitted_changes()?, eq(false));
    let result = ReviewTarget::for_current_branch(&repo);
    let Err(Error::NothingToReview(_)) = result else {
        eyre::bail!("expected nothing to review, got {result:?}");
    };
    Ok(())
}

/// A key stored for one target is never reopened as another, such as a range ending at a branch
/// named `WORKTREE` and the working tree against the same base.
#[gtest]
fn a_key_is_never_reused_for_another_target() -> eyre::Result<()> {
    let test = feature_repo()?;
    test.git(&["branch", "WORKTREE", "feature"])?;
    let repo = Repo::discover(test.path())?;
    let range = ReviewTarget::Range {
        base: "main".to_owned(),
        head: "WORKTREE".to_owned(),
    };
    let worktree = ReviewTarget::Worktree {
        base: "main".to_owned(),
    };
    expect_that!(range.key(&repo)?, eq(&worktree.key(&repo)?));
    Review::open(test.data_dir(), repo.clone(), range)?;
    let result = Review::open(test.data_dir(), repo, worktree);
    let Err(Error::KeyConflict(_)) = result else {
        eyre::bail!("expected a key conflict, got {result:?}");
    };
    Ok(())
}
