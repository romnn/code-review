//! Fetch isolation: a fetch lands in a transient repository of cr's own, and the reviewed
//! repository is never written.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{Error, ObjectId, Repo, Review, ReviewTarget};

use crate::common::{GIT_ENV, TestRepo, files};

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

/// Opens a review of `target` in a depth-1 clone checked out on `feature`, after `prepare`
/// changes the clone, and checks that the merge base was found by deepening, writing nothing to
/// the clone.
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
    let before = files(&local.path().join(".git"))?;

    let data = tempfile::tempdir()?;
    let review = Review::open(data.path(), Repo::discover(local.path())?, target)?;
    let recorded = review.refresh()?;
    assert_that!(recorded.base.as_str(), eq(expected.as_str()));
    assert_that!(files(&local.path().join(".git"))?, eq(&before));
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

/// The transient repository a shallow clone's history is deepened in reaches the remote as the
/// clone does: the clone's configuration forbidding the remote's transport forbids the fetch.
#[test]
fn deepening_follows_the_clones_configuration() -> eyre::Result<()> {
    let remote = remote_with_history()?;
    let local = clone(
        &remote,
        &["--depth=1", "--no-single-branch", "--branch", "feature"],
    )?;
    git(local.path(), &["config", "protocol.file.allow", "never"])?;
    let target = ReviewTarget::Range {
        base: "origin/main".to_owned(),
        head: "feature".to_owned(),
    };
    let data = tempfile::tempdir()?;
    let review = Review::open(data.path(), Repo::discover(local.path())?, target)?;
    assert_that!(
        review.refresh(),
        err(matches_pattern!(Error::NoMergeBase { .. }))
    );
    Ok(())
}

/// Nothing reaches a remote but a fetch, whatever variables an embedder sets: reading a missing
/// blob and detecting a rename across missing blobs fail, and fetch nothing.
#[test]
fn a_partial_clone_reaches_its_remote_only_to_fetch() -> eyre::Result<()> {
    let origin = TestRepo::new()?;
    let mut lines = String::new();
    for line in 1..=40 {
        writeln!(lines, "line {line}")?;
    }
    origin.write("a.txt", &lines)?;
    let base = ObjectId::parse(&origin.commit("base")?)?;
    origin.git(&["mv", "a.txt", "b.txt"])?;
    origin.write("b.txt", &format!("{lines}line 41\n"))?;
    let head = ObjectId::parse(&origin.commit("rename")?)?;
    origin.git(&["config", "uploadpack.allowFilter", "true"])?;
    let blob = ObjectId::parse(&origin.git(&["rev-parse", "HEAD:b.txt"])?)?;
    let local = clone(&origin, &["--filter=blob:none", "--no-checkout"])?;
    let clone = local.path();
    // `rev-list --missing=print` lists the objects the clone lacks without fetching them.
    let missing = || -> eyre::Result<usize> {
        let listed = git(
            clone,
            &["rev-list", "--objects", "--missing=print", "--all"],
        )?;
        Ok(listed.lines().filter(|line| line.starts_with('?')).count())
    };
    assert_that!(missing()?, eq(2));

    // Variables that would have git fetch what is missing, read the origin, which has it, or
    // read another configuration file reach no git.
    let elsewhere = local.path().join("elsewhere.gitconfig");
    std::fs::write(&elsewhere, "")?;
    let mut repo = Repo::discover(clone)?;
    repo.set_command_env([
        ("GIT_NO_LAZY_FETCH", "0".into()),
        ("GIT_DIR", origin.path().join(".git").into_os_string()),
        ("GIT_CONFIG", elsewhere.into_os_string()),
    ]);
    let read = repo.blob_reader()?.read(&blob);
    assert_that!(read, err(matches_pattern!(Error::MissingObject(_))));
    assert!(
        repo.diff_tree(&base, &head).is_err(),
        "the rename was found"
    );
    assert_that!(missing()?, eq(2));
    Ok(())
}

/// Variables set for the repository's commands reach every git it runs, such as the fetch's
/// `ssh`: here they configure a remote that only those commands see.
#[test]
fn command_env_reaches_git() -> eyre::Result<()> {
    let local = TestRepo::new()?;
    let mut repo = Repo::discover(local.path())?;
    assert_that!(repo.remotes()?, is_empty());

    repo.set_command_env([
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "remote.elsewhere.url"),
        ("GIT_CONFIG_VALUE_0", "https://example.com/elsewhere.git"),
    ]);
    let remotes = repo.remotes()?;
    assert_that!(
        remotes
            .iter()
            .map(|remote| (remote.name.as_str(), remote.url.as_str()))
            .collect::<Vec<_>>(),
        elements_are![eq(&("elsewhere", "https://example.com/elsewhere.git"))]
    );

    // Setting them again replaces them.
    repo.set_command_env(Vec::<(String, String)>::new());
    assert_that!(repo.remotes()?, is_empty());
    Ok(())
}

/// A configuration value reads as git resolves it, and an unset key as nothing.
#[test]
fn config_values_read_as_git_resolves_them() -> eyre::Result<()> {
    let local = TestRepo::new()?;
    local.git(&["config", "core.sshCommand", "ssh -i review-key"])?;
    let repo = Repo::discover(local.path())?;
    assert_that!(
        repo.config_value("core.sshCommand")?,
        some(eq("ssh -i review-key"))
    );
    assert_that!(repo.config_value("review.unset")?, none());
    Ok(())
}
