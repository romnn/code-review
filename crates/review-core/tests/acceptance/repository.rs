//! What a repository says about itself: its default branch, whether its working tree holds
//! uncommitted work, its branch's pull request, and the keys of its reviews.

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::indoc;
use review_core::{Error, Repo, Review, ReviewTarget};

use crate::common::TestRepo;

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

/// The default branch is the default remote's, whatever that remote is named, before any local
/// branch: with a lone remote `upstream`, local commits on `main` not yet pushed are not the
/// base.
#[gtest]
fn default_branch_is_the_default_remote_s() -> eyre::Result<()> {
    let upstream = TestRepo::new()?;
    upstream.commit("upstream base")?;
    let test = TestRepo::new()?;
    test.commit("base")?;
    let upstream_path = upstream.path().display().to_string();
    test.git(&["remote", "add", "upstream", &upstream_path])?;
    test.git(&["fetch", "--quiet", "upstream"])?;
    let repo = Repo::discover(test.path())?;
    expect_that!(repo.default_branch()?, some(eq("upstream/main")));

    // `upstream/HEAD` names the remote's default branch, as `origin/HEAD` would.
    upstream.git(&["branch", "trunk"])?;
    test.git(&["fetch", "--quiet", "upstream"])?;
    test.git(&["remote", "set-head", "upstream", "trunk"])?;
    expect_that!(repo.default_branch()?, some(eq("upstream/trunk")));
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

/// Looking up a branch's pull request with a fake `gh`, a shell script, so these run on Unix
/// only.
#[cfg(unix)]
mod pull_requests {
    use std::time::Duration;

    use color_eyre::eyre;
    use googletest::prelude::*;
    use indoc::{formatdoc, indoc};
    use review_core::github::{self, BranchPullRequest, PullRequestState, Repository};
    use review_core::{BranchIdentity, Error, Repo};

    use super::feature_repo;

    /// A repository environment whose `gh` is a shell script running `body`, ahead of `PATH`.
    fn fake_gh(repo: &mut Repo, scratch: &std::path::Path, body: &str) -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let bin = scratch.join("bin");
        std::fs::create_dir_all(&bin)?;
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            formatdoc! {"
            #!/bin/sh
            {body}
        "},
        )?;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755))?;
        let mut path = bin.into_os_string();
        if let Some(inherited) = std::env::var_os("PATH") {
            path.push(":");
            path.push(inherited);
        }
        repo.set_command_env([
            ("PATH", path),
            ("GH_CWD_FILE", scratch.join("gh-cwd").into_os_string()),
        ]);
        Ok(())
    }

    /// What a fake `gh pr view` reports for pull request 7 of `example.invalid/owner/repo`, from
    /// `head` into `develop`.
    fn pull_request_json(head: &str, state: &str) -> String {
        format!(
            r#"{{"number":7,"url":"https://example.invalid/owner/repo/pull/7","state":"{state}","baseRefName":"develop","headRefName":"{head}","headRepository":{{"id":"R","name":"fork"}},"headRepositoryOwner":{{"id":"U","login":"someone"}},"author":{{"login":"someone"}}}}"#
        )
    }

    /// `gh` looks for a bare repository's pull request in that repository, not in whatever checkout
    /// the process runs in.
    #[gtest]
    fn gh_runs_in_a_bare_repository() -> eyre::Result<()> {
        let test = feature_repo()?;
        let scratch = tempfile::tempdir()?;
        let bare_path = scratch.path().join("repo.git");
        let source = test.path().display().to_string();
        let destination = bare_path.display().to_string();
        test.git(&["clone", "--quiet", "--bare", &source, &destination])?;

        // A `gh` that records where it ran and fails.
        let mut repo = Repo::discover(&bare_path)?;
        fake_gh(
            &mut repo,
            scratch.path(),
            indoc! {r#"
                pwd -P > "$GH_CWD_FILE"
                exit 1
            "#},
        )?;
        let found = github::for_checkout(&repo, Duration::from_secs(30));
        expect_that!(found, err(anything()));
        let ran_in = std::fs::read_to_string(scratch.path().join("gh-cwd"))?;
        expect_that!(
            std::path::Path::new(ran_in.trim()),
            eq(bare_path.canonicalize()?.as_path())
        );
        Ok(())
    }

    /// A `gh` that records its arguments and answers with pull request 7 from `head`.
    fn answering_gh(repo: &mut Repo, scratch: &std::path::Path, head: &str) -> eyre::Result<()> {
        let json = pull_request_json(head, "OPEN");
        fake_gh(
            repo,
            scratch,
            &formatdoc! {r#"
                echo "$@" > "$GH_CWD_FILE"
                cat <<'JSON'
                {json}
                JSON
            "#},
        )
    }

    /// The arguments the fake `gh` ran with.
    fn gh_arguments(scratch: &std::path::Path) -> eyre::Result<String> {
        Ok(std::fs::read_to_string(scratch.join("gh-cwd"))?
            .trim()
            .to_owned())
    }

    /// `gh` is asked about the checkout without naming a branch, so it resolves the branch as it
    /// does for the reviewer; its pull request comes back for the branch as it was, and
    /// finding none is no pull request.
    #[gtest]
    fn the_checkout_s_pull_request_comes_back_as_gh_reports_it() -> eyre::Result<()> {
        let test = feature_repo()?;
        let scratch = tempfile::tempdir()?;
        let mut repo = Repo::discover(test.path())?;
        answering_gh(&mut repo, scratch.path(), "feature")?;
        let found = github::for_checkout(&repo, Duration::from_secs(30))?;
        expect_that!(
            found,
            some(matches_pattern!(github::CheckoutPullRequest {
                branch: matches_pattern!(BranchIdentity {
                    branch: eq("feature"),
                    ..
                }),
                pull_request: some(matches_pattern!(BranchPullRequest {
                    number: eq(&7),
                    state: eq(&PullRequestState::Open),
                    base_branch: eq("develop"),
                    base_repository: eq(&Repository {
                        host: "example.invalid".to_owned(),
                        owner: "owner".to_owned(),
                        name: "repo".to_owned(),
                    }),
                    head_branch: eq("feature"),
                    head_repository: some(matches_pattern!(Repository {
                        owner: eq("someone"),
                        name: eq("fork"),
                        ..
                    })),
                    author: some(eq("someone")),
                    ..
                })),
            }))
        );
        expect_that!(
            gh_arguments(scratch.path())?,
            starts_with("pr view --json ")
        );

        fake_gh(
            &mut repo,
            scratch.path(),
            indoc! {r#"
                echo 'no pull requests found for branch "feature"' >&2
                exit 1
            "#},
        )?;
        let nothing = github::for_checkout(&repo, Duration::from_secs(30))?;
        expect_that!(nothing.and_then(|found| found.pull_request), none());
        Ok(())
    }

    /// Where a branch pushes is part of what it is: its own `pushRemote`, or else the
    /// repository's `remote.pushDefault`.
    #[gtest]
    fn a_branch_pushes_where_its_configuration_says() -> eyre::Result<()> {
        let test = feature_repo()?;
        let repo = Repo::discover(test.path())?;
        expect_that!(repo.branch_identity("feature")?.push_remote, none());
        test.git(&["config", "remote.pushDefault", "fork"])?;
        expect_that!(
            repo.branch_identity("feature")?.push_remote.as_deref(),
            some(eq("fork"))
        );
        test.git(&["config", "branch.feature.pushRemote", "mine"])?;
        expect_that!(
            repo.branch_identity("feature")?.push_remote.as_deref(),
            some(eq("mine"))
        );
        Ok(())
    }

    /// A local branch that tracks a remote branch of another name, such as `review-topic`
    /// tracking `topic`, has the pull request of `topic`, whatever `gh` reports as its head.
    #[gtest]
    fn a_branch_tracking_another_name_has_its_pull_request() -> eyre::Result<()> {
        let test = feature_repo()?;
        test.git(&["branch", "--move", "feature", "review-topic"])?;
        test.git(&["config", "branch.review-topic.remote", "origin"])?;
        test.git(&["config", "branch.review-topic.merge", "refs/heads/topic"])?;
        let scratch = tempfile::tempdir()?;
        let mut repo = Repo::discover(test.path())?;
        answering_gh(&mut repo, scratch.path(), "topic")?;
        let found = github::for_checkout(&repo, Duration::from_secs(30))?
            .ok_or_else(|| eyre::eyre!("HEAD is on a branch"))?;
        expect_that!(found.branch.branch, eq("review-topic"));
        expect_that!(found.branch.merge.as_deref(), some(eq("refs/heads/topic")));
        expect_that!(found.pull_request.map(|found| found.number), some(eq(7)));
        Ok(())
    }

    /// A branch renamed after its pull request was opened keeps it: it still pulls the branch
    /// the pull request is from.
    #[gtest]
    fn a_renamed_branch_keeps_its_pull_request() -> eyre::Result<()> {
        let test = feature_repo()?;
        test.git(&["config", "branch.feature.remote", "origin"])?;
        test.git(&["config", "branch.feature.merge", "refs/heads/feature"])?;
        test.git(&["branch", "--move", "feature", "feature-renamed"])?;
        let scratch = tempfile::tempdir()?;
        let mut repo = Repo::discover(test.path())?;
        answering_gh(&mut repo, scratch.path(), "feature")?;
        let found = github::for_checkout(&repo, Duration::from_secs(30))?
            .ok_or_else(|| eyre::eyre!("HEAD is on a branch"))?;
        expect_that!(found.branch.branch, eq("feature-renamed"));
        expect_that!(found.pull_request.map(|found| found.number), some(eq(7)));
        Ok(())
    }

    /// A branch that `gh pr checkout` made, which tracks the pull request's own ref, has that
    /// pull request.
    #[gtest]
    fn a_branch_tracking_a_pull_request_ref_has_its_pull_request() -> eyre::Result<()> {
        let test = feature_repo()?;
        test.git(&["checkout", "--quiet", "-b", "someone-feature"])?;
        test.git(&["config", "branch.someone-feature.remote", "origin"])?;
        test.git(&["config", "branch.someone-feature.merge", "refs/pull/7/head"])?;
        let scratch = tempfile::tempdir()?;
        let mut repo = Repo::discover(test.path())?;
        answering_gh(&mut repo, scratch.path(), "feature")?;
        let found = github::for_checkout(&repo, Duration::from_secs(30))?
            .ok_or_else(|| eyre::eyre!("HEAD is on a branch"))?;
        expect_that!(found.branch.merge.as_deref(), some(eq("refs/pull/7/head")));
        expect_that!(found.pull_request.map(|found| found.number), some(eq(7)));
        Ok(())
    }

    /// An answer during which the branch was pointed elsewhere may be for another, so it is
    /// refused; while `HEAD` is detached, `gh` is not asked at all.
    #[gtest]
    fn a_lookup_during_which_the_branch_moved_is_refused() -> eyre::Result<()> {
        let test = feature_repo()?;
        let scratch = tempfile::tempdir()?;
        let mut repo = Repo::discover(test.path())?;
        let json = pull_request_json("feature", "OPEN");
        fake_gh(
            &mut repo,
            scratch.path(),
            &formatdoc! {r"
                git config branch.feature.merge refs/heads/elsewhere
                cat <<'JSON'
                {json}
                JSON
            "},
        )?;
        expect_that!(
            github::for_checkout(&repo, Duration::from_secs(30)),
            err(matches_pattern!(Error::Raced(_)))
        );

        // A branch switched away and back while `gh` ran looks the same before and after, but
        // `gh` answered for the other: a pull request from another head than the branch pulls
        // is not its own.
        test.git(&["config", "branch.feature.merge", "refs/heads/feature"])?;
        answering_gh(&mut repo, scratch.path(), "elsewhere")?;
        expect_that!(
            github::for_checkout(&repo, Duration::from_secs(30)),
            err(matches_pattern!(Error::Raced(_)))
        );
        test.git(&["config", "branch.feature.merge", "refs/pull/8/head"])?;
        answering_gh(&mut repo, scratch.path(), "feature")?;
        expect_that!(
            github::for_checkout(&repo, Duration::from_secs(30)),
            err(matches_pattern!(Error::Raced(_)))
        );
        test.git(&["config", "--unset", "branch.feature.merge"])?;
        answering_gh(&mut repo, scratch.path(), "other-name")?;
        expect_that!(
            github::for_checkout(&repo, Duration::from_secs(30)),
            err(matches_pattern!(Error::Raced(_)))
        );

        std::fs::remove_file(scratch.path().join("gh-cwd"))?;
        test.git(&["checkout", "--quiet", "--detach"])?;
        answering_gh(&mut repo, scratch.path(), "feature")?;
        expect_that!(
            github::for_checkout(&repo, Duration::from_secs(30))?,
            none()
        );
        expect_false!(scratch.path().join("gh-cwd").exists());
        Ok(())
    }

    /// A `gh` that hangs is stopped once its time is up.
    #[gtest]
    fn a_pull_request_lookup_is_bounded() -> eyre::Result<()> {
        let test = feature_repo()?;
        let scratch = tempfile::tempdir()?;
        let mut repo = Repo::discover(test.path())?;
        fake_gh(&mut repo, scratch.path(), "exec sleep 30")?;
        let started = std::time::Instant::now();
        let found = github::for_checkout(&repo, Duration::from_millis(200));
        expect_that!(found, err(matches_pattern!(Error::TimedOut { .. })));
        expect_that!(started.elapsed(), lt(Duration::from_secs(10)));
        Ok(())
    }
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
    expect_that!(test.uncommitted()?, eq(true));

    // An ignored file is not work to review.
    test.git(&["config", "status.showUntrackedFiles", "normal"])?;
    test.write(".git/info/exclude", "new.rs\n")?;
    expect_that!(test.uncommitted()?, eq(false));
    Ok(())
}

/// A capture never runs git in a submodule's checkout to see whether it is dirty: a filter
/// driver configured only in the submodule, which turning off the superproject's drivers does
/// not reach, would run there.
#[cfg(unix)]
#[gtest]
fn a_capture_runs_nothing_in_a_submodule() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("sub/inner.rs", "fn inner() {}\n")?;
    test.write("sub/.gitattributes", "*.rs filter=mark\n")?;
    let sub = test.path().join("sub");
    let marker = test.path().join("filter-ran");
    let clean = format!("sh -c 'touch {}; cat'", marker.display());
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["add", "--all"],
        &["commit", "--quiet", "--no-verify", "-m", "inner"],
        &["config", "filter.mark.clean", &clean],
    ] {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&sub)
            .args(args)
            .envs(crate::common::GIT_ENV)
            .status()?;
        eyre::ensure!(status.success(), "git {} failed in sub", args.join(" "));
    }
    test.write("lib.rs", "fn lib() {}\n")?;
    test.commit("base with an embedded repository")?;
    // The same bytes again: git cannot tell the file unchanged without reading it, through
    // the filter.
    test.write("sub/inner.rs", "fn inner() {}\n")?;
    test.write("lib.rs", "fn lib() { edited(); }\n")?;
    test.capture()?;
    expect_false!(marker.exists());
    Ok(())
}

/// Changes inside a submodule's own checkout are not uncommitted work of the superproject: a
/// capture records only the submodule's commit.
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
    expect_that!(test.uncommitted()?, eq(false));
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
