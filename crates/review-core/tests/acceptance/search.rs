//! Tree listings and content search over a pinned commit.

use std::sync::atomic::AtomicBool;

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::git::GrepLine;
use review_core::{ObjectId, Repo};

use crate::common::TestRepo;

/// The listing names every blob of the commit, including nested ones, and nothing else.
#[gtest]
fn tree_files_lists_every_blob() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    repo.write("src/nested/b.rs", "fn b() {}\n")?;
    let commit = ObjectId::parse(&repo.commit("init")?)?;
    repo.write("later.txt", "not in the commit\n")?;

    let files = Repo::discover(repo.path())?.tree_files(&commit)?;
    let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();

    expect_that!(paths, elements_are![eq(&"a.txt"), eq(&"src/nested/b.rs")]);
    Ok(())
}

/// Grep searches the pinned commit rather than the working tree, as a fixed string, with
/// zero-based line numbers.
#[gtest]
fn grep_searches_the_commit_as_a_fixed_string() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "alpha\nneedle (x)\nomega\n")?;
    repo.write("dir/b.txt", "Needle (x) twice\n")?;
    let commit = ObjectId::parse(&repo.commit("init")?)?;
    repo.write("a.txt", "needle (x) only in the working tree\n")?;
    let git = Repo::discover(repo.path())?;
    let cancel = AtomicBool::new(false);

    let exact = git.grep(&commit, "needle (x)", false, 100, &cancel)?;
    let folded = git.grep(&commit, "needle (x)", true, 100, &cancel)?;
    let none = git.grep(&commit, "absent", false, 100, &cancel)?;
    let limited = git.grep(&commit, "needle", true, 1, &cancel)?;

    expect_that!(
        exact,
        elements_are![eq(&GrepLine {
            path: "a.txt".to_owned(),
            line: 1,
            text: "needle (x)".to_owned(),
        })]
    );
    expect_that!(
        folded
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        elements_are![eq(&"a.txt"), eq(&"dir/b.txt")]
    );
    expect_that!(none.len(), eq(0));
    expect_that!(limited.len(), eq(1));
    Ok(())
}

/// A search cancelled before it starts returns nothing.
#[gtest]
fn grep_stops_when_cancelled() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "needle\n")?;
    let commit = ObjectId::parse(&repo.commit("init")?)?;
    let cancel = AtomicBool::new(true);

    let hits = Repo::discover(repo.path())?.grep(&commit, "needle", false, 100, &cancel)?;

    expect_that!(hits.len(), eq(0));
    Ok(())
}

/// A grep that git cannot run, here on an object that is not a commit, is an error rather than
/// an empty result.
#[gtest]
fn grep_reports_a_failing_git() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "needle\n")?;
    repo.commit("init")?;
    let missing = ObjectId::parse("0123456789abcdef0123456789abcdef01234567")?;
    let cancel = AtomicBool::new(false);

    let result = Repo::discover(repo.path())?.grep(&missing, "needle", false, 100, &cancel);

    expect_that!(result.is_err(), eq(true));
    Ok(())
}

/// A path that is not valid UTF-8 is left out of the listing and the hits instead of failing
/// the whole tree.
#[cfg(target_os = "linux")]
#[gtest]
fn non_utf8_paths_are_skipped() -> eyre::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let repo = TestRepo::new()?;
    repo.write("good.txt", "needle\n")?;
    std::fs::write(
        repo.path()
            .join(std::ffi::OsStr::from_bytes(b"bad\xff.txt")),
        "needle\n",
    )?;
    let commit = ObjectId::parse(&repo.commit("init")?)?;
    let git = Repo::discover(repo.path())?;
    let cancel = AtomicBool::new(false);

    let files = git.tree_files(&commit)?;
    let hits = git.grep(&commit, "needle", false, 100, &cancel)?;

    expect_that!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        elements_are![eq(&"good.txt")]
    );
    expect_that!(
        hits.iter().map(|hit| hit.path.as_str()).collect::<Vec<_>>(),
        elements_are![eq(&"good.txt")]
    );
    Ok(())
}
