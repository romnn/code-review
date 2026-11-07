//! When a review last synced with its target, and the steps a refresh reports.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::{RefreshStep, RevisionNumber};

use crate::common::TestRepo;

/// A refresh stores when it synced and with which revision current, also when nothing changed,
/// and reports analysis only for a snapshot pair it has not analyzed yet.
#[test]
fn refreshes_record_when_they_synced() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write("a.txt", "two\n")?;
    repo.commit("change")?;
    let review = repo.review()?;
    assert_that!(review.synced()?, none());

    let mut steps = Vec::new();
    let recorded = review.refresh_reporting(&mut |step| steps.push(step))?;
    assert_that!(
        steps,
        elements_are![
            eq(&RefreshStep::Fetching),
            eq(&RefreshStep::Analyzing),
            eq(&RefreshStep::Describing)
        ]
    );
    let synced = review
        .synced()?
        .ok_or_else(|| eyre::eyre!("a sync record"))?;
    assert_that!(synced.revision, eq(recorded.number));
    assert_that!(synced.revision, eq(RevisionNumber::FIRST));

    // Nothing new: the record is written again, and nothing is analyzed.
    let mut again = Vec::new();
    review.refresh_reporting(&mut |step| again.push(step))?;
    assert_that!(
        again,
        elements_are![eq(&RefreshStep::Fetching), eq(&RefreshStep::Describing)]
    );
    let resynced = review
        .synced()?
        .ok_or_else(|| eyre::eyre!("a sync record"))?;
    assert_that!(resynced.revision, eq(RevisionNumber::FIRST));
    assert_that!(resynced.at, ge(synced.at));
    Ok(())
}

/// The sync record only informs the reader: when it cannot be stored, the refresh still
/// records the revision, describes the review, and says why in the outcome.
#[test]
fn a_sync_record_that_cannot_be_written_fails_nothing() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.txt", "one\n")?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write("a.txt", "two\n")?;
    repo.commit("change")?;
    let review = repo.review()?;
    // A directory where the record goes makes renaming the new record over it fail.
    std::fs::create_dir_all(review.dir().synced())?;

    let mut steps = Vec::new();
    let recorded = review.refresh_reporting(&mut |step| steps.push(step))?;
    assert_that!(recorded.created, eq(true));
    assert_that!(recorded.number, eq(RevisionNumber::FIRST));
    assert_that!(recorded.sync_error, some(contains_substring("synced.json")));
    // The review's description still ran after the failed record.
    assert_that!(steps.last(), some(eq(&RefreshStep::Describing)));
    assert_that!(recorded.info_error, none());
    assert_that!(review.info()?, some(anything()));
    assert_that!(review.history()?.current(), some(eq(RevisionNumber::FIRST)));
    Ok(())
}
