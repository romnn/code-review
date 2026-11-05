//! What a review is about: stored at refresh, read back by the front ends.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::ReviewInfo;
use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};

use crate::common::{TestRepo, refresh};

/// A range review stores its commits, newest first, with subjects and bodies.
#[test]
fn range_reviews_store_their_commits() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", &base)?;
    repo.write("a.txt", "two\n")?;
    repo.commit("Change a\n\nBecause two is better.")?;
    repo.write("b.txt", "new\n")?;
    repo.commit("Add b")?;
    let review = repo.review()?;
    assert_that!(review.info()?, none());
    refresh(&review)?;

    let Some(ReviewInfo::Commits { commits }) = review.info()? else {
        eyre::bail!("expected commits");
    };
    let subjects: Vec<&str> = commits
        .iter()
        .map(|commit| commit.subject.as_str())
        .collect();
    assert_that!(subjects, eq(&vec!["Add b", "Change a"]));
    assert_that!(commits[1].body, eq("Because two is better."));
    Ok(())
}

/// The demo plays a pull request with a title, a Markdown description and labels.
#[test]
fn the_demo_is_a_pull_request() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let review = prepare(
        &DemoPaths::under(dir.path()),
        &fixture_dir(),
        DemoRequest::Fresh,
    )?;
    let Some(ReviewInfo::PullRequest(pr)) = review.info()? else {
        eyre::bail!("expected a pull request");
    };
    assert_that!(pr.number, eq(128));
    assert!(pr.body.contains("## Summary"));
    assert_that!(pr.labels.len(), eq(2));
    Ok(())
}
