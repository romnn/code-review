//! Proven formatting-only units approved automatically: derived per revision from the proof,
//! never written as marks, and always overruled by a mark on the unit's chain.

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::indoc;
use review_core::{MarkVerdict, ReviewPolicy, Standing, UnitState, Verdict};

use crate::common::{TestRepo, find, refresh, summaries};

/// The policy of a front end that approves proven formatting-only units automatically.
const AUTO_APPROVE: ReviewPolicy = ReviewPolicy {
    auto_approve_formatting: true,
};

/// A Rust file whose call re-wraps (proven formatting-only) beside one real edit.
const BEFORE: &str = indoc! {"
    fn main() {
        let total = compute(alpha, beta, gamma);
        let other = 0;
        let limit = 1;
        run(total, limit);
    }
"};
const AFTER: &str = indoc! {"
    fn main() {
        let total = compute(
            alpha,
            beta,
            gamma
        );
        let other = 0;
        let limit = 2;
        run(total, limit);
    }
"};

/// A repository whose `feature` re-wraps a call and makes one real edit, and its review at
/// revision 1.
fn rewrap_and_edit() -> eyre::Result<(TestRepo, review_core::Review)> {
    let repo = TestRepo::new()?;
    repo.write("src/lib.rs", BEFORE)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write("src/lib.rs", AFTER)?;
    repo.commit("re-wrap and edit")?;
    let review = repo.review()?;
    refresh(&review)?;
    Ok((repo, review))
}

/// The re-wrap counts as approved automatically and the edit still needs review, with no mark
/// written for the re-wrap: its recorded state stays new.
#[test]
fn formatting_only_units_count_as_approved_automatically() -> eyre::Result<()> {
    let (_repo, review) = rewrap_and_edit()?;
    let history = review.history()?;
    let view = history.current_view_under(AUTO_APPROVE)?;
    let units = summaries(&history, view.number())?;
    let rewrap = find(&units, "compute(")?;
    let edit = find(&units, "limit = 2")?;

    assert_that!(view.standing(rewrap.id)?, eq(Standing::AutoApproved));
    assert_that!(view.standing(edit.id)?, eq(Standing::Open));
    // Nothing was recorded on the reviewer's behalf.
    assert_that!(rewrap.state, eq(UnitState::New));

    let total = view.status().total;
    assert_that!(total.units, eq(2));
    assert_that!(total.auto_approved, eq(1));
    assert_that!(total.new, eq(1));
    assert_that!(total.unreviewed(), eq(1));
    assert_that!(total.approved(), eq(1));
    Ok(())
}

/// A flag on a formatting-only unit keeps it flagged rather than approved automatically.
#[test]
fn an_explicit_flag_wins() -> eyre::Result<()> {
    let (_repo, review) = rewrap_and_edit()?;
    let history = review.history()?;
    let view = history.current_view_under(AUTO_APPROVE)?;
    let rewrap = find(&summaries(&history, view.number())?, "compute(")?.id;
    review.mark(&view, &[rewrap], MarkVerdict::Flagged)?;

    let history = review.history()?;
    let view = history.current_view_under(AUTO_APPROVE)?;
    assert_that!(
        view.standing(rewrap)?,
        eq(Standing::Reviewed(Verdict::Flagged))
    );
    let total = view.status().total;
    assert_that!(total.flagged, eq(1));
    assert_that!(total.auto_approved, eq(0));
    Ok(())
}

/// An explicit reset makes a formatting-only unit need review again.
#[test]
fn an_explicit_reset_wins() -> eyre::Result<()> {
    let (_repo, review) = rewrap_and_edit()?;
    let history = review.history()?;
    let view = history.current_view_under(AUTO_APPROVE)?;
    let rewrap = find(&summaries(&history, view.number())?, "compute(")?.id;
    review.mark(&view, &[rewrap], MarkVerdict::Unseen)?;

    let history = review.history()?;
    let view = history.current_view_under(AUTO_APPROVE)?;
    assert_that!(view.standing(rewrap)?, eq(Standing::Open));
    let total = view.status().total;
    assert_that!(total.new, eq(2));
    assert_that!(total.auto_approved, eq(0));
    Ok(())
}

/// Turning the policy off makes every unit need a mark, and a view built without a policy,
/// as an embedder that never opted in builds it, decides by marks alone.
#[test]
fn the_policy_can_turn_it_off() -> eyre::Result<()> {
    let (_repo, review) = rewrap_and_edit()?;
    let history = review.history()?;
    let marks_only = history.current_view_under(ReviewPolicy::MARKS_ONLY)?;
    let rewrap = find(&summaries(&history, marks_only.number())?, "compute(")?.id;
    for view in [marks_only, history.current_view()?] {
        assert_that!(view.policy(), eq(ReviewPolicy::MARKS_ONLY));
        assert_that!(view.standing(rewrap)?, eq(Standing::Open));
        let total = view.status().total;
        assert_that!(total.new, eq(2));
        assert_that!(total.auto_approved, eq(0));
    }
    // Walkthrough progress and next-open navigation, as the Zed fork reads them, count the
    // re-wrap as open too.
    let view = history.current_view()?;
    let partition = review_core::walkthrough::partition(&view);
    let reviewed: usize = partition
        .progress(&view)
        .iter()
        .map(|step| step.reviewed)
        .sum();
    assert_that!(reviewed, eq(0));
    let mut open = Vec::new();
    let mut from = None;
    while let Some(next) = partition.next_unreviewed(&view, from) {
        if open.contains(&next) {
            break;
        }
        open.push(next);
        from = Some(next);
    }
    assert_that!(open, contains(eq(&rewrap)));
    Ok(())
}

/// A whitespace change the proof cannot show equivalent, such as dedenting a Python statement
/// out of its block, is never approved automatically.
#[test]
fn unproven_whitespace_changes_are_not_approved() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write(
        "main.py",
        indoc! {"
            if ready:
                start()
                finish()
        "},
    )?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "main.py",
        indoc! {"
            if ready:
                start()
            finish()
        "},
    )?;
    repo.commit("dedent")?;
    let review = repo.review()?;
    refresh(&review)?;
    let history = review.history()?;
    let view = history.current_view_under(AUTO_APPROVE)?;
    let units = summaries(&history, view.number())?;
    let dedent = find(&units, "finish()")?;
    // The chunk changes only whitespace, but that is not proven to mean the same.
    assert!(
        dedent
            .unit
            .chunk()
            .is_some_and(|chunk| chunk.whitespace_only)
    );
    assert!(!dedent.unit.formatting_only);
    assert_that!(view.standing(dedent.id)?, eq(Standing::Open));
    assert_that!(view.status().total.auto_approved, eq(0));
    Ok(())
}

/// Automatic approval is decided again in every revision from that revision's proof: once a
/// push turns the re-wrap into a real edit, it needs review, since no mark was ever carried.
#[test]
fn a_revision_without_the_proof_needs_review() -> eyre::Result<()> {
    let (repo, review) = rewrap_and_edit()?;
    let first = review
        .history()?
        .current()
        .ok_or_else(|| eyre::eyre!("a revision"))?;

    repo.write(
        "src/lib.rs",
        &AFTER.replace("        beta,\n", "        delta,\n"),
    )?;
    repo.commit("edit inside the re-wrap")?;
    let second = refresh(&review)?;
    assert_that!(second, not(eq(first)));

    let history = review.history()?;
    let view = history.view_under(second, AUTO_APPROVE)?;
    let units = summaries(&history, second)?;
    let rewrap = find(&units, "delta")?;
    assert!(!rewrap.unit.formatting_only);
    assert_that!(view.standing(rewrap.id)?, eq(Standing::Open));
    assert_that!(view.status().total.auto_approved, eq(0));
    // The first revision still reads as it did.
    assert_that!(
        history
            .view_under(first, AUTO_APPROVE)?
            .status()
            .total
            .auto_approved,
        eq(1)
    );
    Ok(())
}
