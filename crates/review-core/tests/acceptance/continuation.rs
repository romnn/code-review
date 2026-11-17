//! Continuing a range or pull request review in a checkout of its branch: the checkout's working
//! tree, starting from the review's current revision and its marks, while the review itself
//! keeps only what its target resolves to.
//!
//! These repositories have no GitHub remote, so the continued reviews are ranges; a pull request
//! review is continued the same way once its head branch is known.

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::formatdoc;
use review_core::{Detected, MarkVerdict, Repo, Review, UnitState, Verdict};

use crate::common::{TestRepo, find, mark_all, refresh, summaries};

/// Three functions apart from each other, with the bodies given.
fn three_functions(a: &str, b: &str, c: &str) -> String {
    formatdoc! {"
        fn a() {{ {a} }}

        fn keep_one() {{}}

        fn b() {{ {b} }}

        fn keep_two() {{}}

        fn c() {{ {c} }}
    "}
}

/// A repository on `feature`, which changes `a`, `b` and `c` over `main`, with the review of
/// `main..feature` recorded, every unit approved, and remembered for the branch.
fn reviewed_feature() -> eyre::Result<(TestRepo, Review)> {
    let test = TestRepo::new()?;
    test.write("lib.rs", &three_functions("", "", ""))?;
    test.commit("base")?;
    test.branch_at("feature", "main")?;
    test.write("lib.rs", &three_functions("one();", "two();", "three();"))?;
    test.commit("feature")?;
    let review = test.review()?;
    let revision = refresh(&review)?;
    mark_all(&review, revision, MarkVerdict::Accepted)?;
    review_core::store::remember_review(test.data_dir(), review.repo(), review.key())?;
    Ok((test, review))
}

/// Opens without a target, as `cr` does, and remembers what opened, as the apps do.
fn open(test: &TestRepo) -> eyre::Result<(Review, Detected)> {
    let (review, detected) =
        Review::open_for_current_branch(test.data_dir(), Repo::discover(test.path())?)?;
    review_core::store::remember_review(test.data_dir(), review.repo(), review.key())?;
    Ok((review, detected))
}

/// An uncommitted edit in a checkout of the reviewed branch shows in the continuation: only the
/// edited unit is to review again, the others keep their approval, and the range review itself
/// is unchanged.
#[gtest]
fn a_checkout_continues_the_review_of_its_branch() -> eyre::Result<()> {
    let (test, range) = reviewed_feature()?;
    test.write(
        "lib.rs",
        &three_functions("one();", "two(); more();", "three();"),
    )?;

    let (continuation, detected) = open(&test)?;
    expect_that!(
        detected,
        matches_pattern!(Detected::Continued {
            label: eq("main..feature"),
            created: eq(&true),
            ..
        })
    );
    expect_that!(continuation.key(), not(eq(range.key())));
    expect_that!(
        continuation
            .continues()
            .map(|continues| continues.review.clone()),
        some(eq(range.key()))
    );

    let revision = refresh(&continuation)?;
    expect_that!(revision.get(), eq(2));
    let units = summaries(&continuation.history()?, revision)?;
    expect_that!(
        find(&units, "one();")?.state,
        matches_pattern!(UnitState::Reviewed {
            verdict: eq(Verdict::Accepted),
            inherited: eq(true),
            ..
        })
    );
    expect_that!(
        find(&units, "three();")?.state,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    expect_that!(
        find(&units, "more();")?.state,
        matches_pattern!(UnitState::ChangedSinceReviewed { .. })
    );

    // The range review records only what its range resolves to.
    let history = range.history()?;
    expect_that!(
        history.current().map(review_core::RevisionNumber::get),
        some(eq(1))
    );
    Ok(())
}

/// A continuation opens again as it is: remembered, and also when the continued review is
/// opened by name in between and the checkout no longer contains the revision it continued.
#[gtest]
fn a_continuation_is_reopened_as_it_is() -> eyre::Result<()> {
    let (test, range) = reviewed_feature()?;
    let (first, _) = open(&test)?;
    refresh(&first)?;

    let (again, detected) = open(&test)?;
    expect_that!(detected, eq(&Detected::Remembered));
    expect_that!(again.key(), eq(first.key()));

    // Naming the range remembers it, and a rewrite of the branch leaves the checkout without
    // the head the continuation started from; the continuation still opens, not seeded again.
    review_core::store::remember_review(test.data_dir(), range.repo(), range.key())?;
    test.git(&[
        "commit",
        "--amend",
        "--quiet",
        "--message",
        "feature, rewritten",
    ])?;
    let (reopened, detected) = open(&test)?;
    expect_that!(reopened.key(), eq(first.key()));
    expect_that!(
        detected,
        matches_pattern!(Detected::Continued {
            created: eq(&false),
            ..
        })
    );
    Ok(())
}

/// The seed copies each unit's state, not what the policy shows: a unit reset to unseen stays
/// to review, and the continued review's trust decision comes along.
#[gtest]
fn resets_and_trust_carry_into_the_seed() -> eyre::Result<()> {
    let (test, range) = reviewed_feature()?;
    let history = range.history()?;
    let view = history.current_view()?;
    let units = summaries(&history, view.number())?;
    range.mark(&view, &[find(&units, "two();")?.id], MarkVerdict::Unseen)?;
    range.set_trusted(false)?;

    let (continuation, _) = open(&test)?;
    let history = continuation.history()?;
    let units = summaries(&history, history.current().unwrap())?;
    expect_that!(
        find(&units, "two();")?.state,
        matches_pattern!(UnitState::Unseen { .. })
    );
    expect_that!(
        find(&units, "one();")?.state,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    let trust = continuation.trust()?;
    expect_that!((trust.trusted, trust.decided), eq((false, true)));
    Ok(())
}

/// A checkout whose `HEAD` does not contain the reviewed revision is not followed: the review
/// itself opens, and says why.
#[gtest]
fn a_checkout_without_the_reviewed_head_is_not_followed() -> eyre::Result<()> {
    let (test, range) = reviewed_feature()?;
    test.git(&[
        "commit",
        "--amend",
        "--quiet",
        "--message",
        "feature, rewritten",
    ])?;

    let (opened, detected) = open(&test)?;
    expect_that!(opened.key(), eq(range.key()));
    let Detected::Unfollowed { why, .. } = detected else {
        panic!("expected the checkout not to be followed, got {detected:?}");
    };
    expect_that!(
        why,
        contains_substring("does not contain revision 1's head")
    );
    Ok(())
}

/// A revision pins its target, which a target branch that moved past the merge base keeps
/// reachable from neither snapshot, and a continuation compares from that same target.
#[gtest]
fn the_target_is_pinned_and_continued_from() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write("lib.rs", &three_functions("", "", ""))?;
    test.commit("base")?;
    test.branch_at("feature", "main")?;
    test.switch("main")?;
    test.write("other.rs", "fn main_moved_on() {}\n")?;
    test.commit("main moves on")?;
    test.switch("feature")?;
    test.write("lib.rs", &three_functions("one();", "", ""))?;
    test.commit("feature")?;
    let range = test.review()?;
    let revision = refresh(&range)?;
    mark_all(&range, revision, MarkVerdict::Accepted)?;
    review_core::store::remember_review(test.data_dir(), range.repo(), range.key())?;
    let history = range.history()?;
    let record = history.view(revision)?.record().cloned().unwrap();
    let target = record.target.clone().unwrap();
    expect_that!(target, not(eq(&record.base)));
    expect_that!(
        range.repo().ref_target(&range.pin_ref(&target))?,
        some(eq(&target))
    );

    let (continuation, _) = open(&test)?;
    expect_that!(
        continuation
            .repo()
            .ref_target(&continuation.pin_ref(&target))?,
        some(eq(&target))
    );
    let revision = refresh(&continuation)?;
    let units = summaries(&continuation.history()?, revision)?;
    expect_that!(units.len(), eq(1));
    expect_that!(
        find(&units, "one();")?.state,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    Ok(())
}
