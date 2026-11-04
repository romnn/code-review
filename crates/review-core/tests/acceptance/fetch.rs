//! Fetch isolation: the engine never updates refs outside `refs/code-review/*` or `FETCH_HEAD`.

use std::path::Path;
use std::process::Command;

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{ObjectId, Repo, Review, ReviewTarget};

use crate::common::{GIT_ENV, TestRepo};

fn git(directory: &Path, args: &[&str]) -> eyre::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .envs(GIT_ENV)
        .output()?;
    eyre::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Every ref with its target, plus the content of `FETCH_HEAD`.
fn ref_state(directory: &Path) -> eyre::Result<Vec<String>> {
    let refs = git(
        directory,
        &["for-each-ref", "--format=%(refname) %(objectname)"],
    )?;
    let mut state: Vec<String> = refs.lines().map(str::to_owned).collect();
    let fetch_head = std::fs::read_to_string(directory.join(".git/FETCH_HEAD")).unwrap_or_default();
    state.push(format!("FETCH_HEAD {fetch_head}"));
    Ok(state)
}

/// A remote with `main` and `feature` sharing an early base, ten commits deep each.
fn remote_with_history() -> eyre::Result<TestRepo> {
    let remote = TestRepo::new()?;
    remote.write("base.txt", "base\n")?;
    remote.commit("base")?;
    remote.branch_at("feature", "main")?;
    for index in 0..10 {
        remote.write("feature.txt", &format!("{index}\n"))?;
        remote.commit(&format!("feature {index}"))?;
    }
    remote.switch("main")?;
    for index in 0..10 {
        remote.write("main.txt", &format!("{index}\n"))?;
        remote.commit(&format!("main {index}"))?;
    }
    Ok(remote)
}

fn clone(remote: &TestRepo, extra: &[&str]) -> eyre::Result<tempfile::TempDir> {
    let local = tempfile::tempdir()?;
    let url = format!("file://{}", remote.path().display());
    let destination = local.path().display().to_string();
    let mut args = vec!["clone", "--quiet"];
    args.extend_from_slice(extra);
    args.extend([url.as_str(), destination.as_str()]);
    git(remote.path(), &args)?;
    Ok(local)
}

/// Assert that between two ref states only refs under `refs/code-review/` changed.
fn assert_only_review_refs_changed(before: &[String], after: &[String]) {
    let outside = |state: &[String]| -> Vec<String> {
        state
            .iter()
            .filter(|line| !line.starts_with("refs/code-review/"))
            .cloned()
            .collect()
    };
    assert_that!(outside(after), eq(&outside(before)));
}

/// Fetching into temporary refs leaves remote-tracking refs and `FETCH_HEAD` alone, although
/// `origin` has the usual configured fetch refspec.
#[test]
fn fetch_updates_only_review_refs() -> eyre::Result<()> {
    let remote = remote_with_history()?;
    let local = clone(&remote, &[])?;
    remote.write("main.txt", "moved on\n")?;
    let moved = remote.commit("main moves on")?;

    let before = ref_state(local.path())?;
    let repo = Repo::discover(local.path())?;
    repo.fetch(
        "origin",
        &[
            "+refs/heads/main:refs/code-review/tmp/test-target".to_owned(),
            "+refs/heads/feature:refs/code-review/tmp/test-head".to_owned(),
        ],
    )?;
    let after = ref_state(local.path())?;
    assert_only_review_refs_changed(&before, &after);
    assert_that!(
        repo.resolve_commit("refs/code-review/tmp/test-target")?
            .as_str(),
        eq(moved.as_str())
    );
    Ok(())
}

/// Deepening a shallow clone to find the merge base changes no ref and no `FETCH_HEAD`.
#[test]
fn shallow_deepening_updates_no_refs() -> eyre::Result<()> {
    let remote = remote_with_history()?;
    let local = clone(&remote, &["--depth=1", "--no-single-branch"])?;
    let target = ObjectId::parse(&git(local.path(), &["rev-parse", "origin/main"])?)?;
    let head = ObjectId::parse(&git(local.path(), &["rev-parse", "origin/feature"])?)?;
    remote.write("main.txt", "moved on\n")?;
    remote.commit("main moves on")?;

    let before = ref_state(local.path())?;
    let repo = Repo::discover(local.path())?;
    let bases = repo.merge_bases(&target, &head, Some("origin"))?;
    let expected = git(
        remote.path(),
        &["merge-base", target.as_str(), head.as_str()],
    )?;
    assert_that!(
        bases.first().map(ObjectId::as_str),
        some(eq(expected.as_str()))
    );
    assert_that!(ref_state(local.path())?, eq(&before));
    Ok(())
}

/// Opens a review of `target` in a depth-1 clone checked out on `feature`, after `prepare`
/// changes the clone, and checks that the merge base was found by deepening without touching
/// refs outside `refs/code-review/*`.
fn review_in_shallow_clone(
    target: ReviewTarget,
    prepare: impl Fn(&Path) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let remote = remote_with_history()?;
    let local = clone(
        &remote,
        &["--depth=1", "--no-single-branch", "--branch", "feature"],
    )?;
    prepare(local.path())?;
    let expected = git(remote.path(), &["merge-base", "main", "feature"])?;
    let before = ref_state(local.path())?;

    let data = tempfile::tempdir()?;
    let review = Review::open(data.path(), Repo::discover(local.path())?, target)?;
    let recorded = review.refresh()?;
    assert_that!(recorded.base.as_str(), eq(expected.as_str()));
    assert_only_review_refs_changed(&before, &ref_state(local.path())?);
    Ok(())
}

/// A working-tree review in a shallow clone deepens the published history; it never asks the
/// remote for the synthetic snapshot commit.
#[test]
fn shallow_worktree_review_finds_merge_base() -> eyre::Result<()> {
    review_in_shallow_clone(
        ReviewTarget::Worktree {
            base: "origin/main".to_owned(),
        },
        |local| Ok(std::fs::write(local.join("feature.txt"), "uncommitted\n")?),
    )
}

/// A range review whose head is an unpublished local commit in a shallow clone finds the merge
/// base too.
#[test]
fn shallow_review_with_unpublished_head_finds_merge_base() -> eyre::Result<()> {
    review_in_shallow_clone(
        ReviewTarget::Range {
            base: "origin/main".to_owned(),
            head: "feature".to_owned(),
        },
        |local| {
            std::fs::write(local.join("feature.txt"), "local only\n")?;
            git(local, &["commit", "--quiet", "--all", "-m", "unpublished"])?;
            Ok(())
        },
    )
}
