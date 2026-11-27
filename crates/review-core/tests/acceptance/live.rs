//! Live working-tree reviews: the working tree as captured now shows as a provisional revision,
//! recorded only once something refers to it.

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::formatdoc;
use review_core::{
    Capture, Error, History, MarkVerdict, Repo, Review, ReviewTarget, RevisionRecord, UnitState,
};

use crate::common::{TestRepo, find, summaries};

/// A file of two functions, `a` and `b`, with the bodies given.
fn two_functions(a: &str, b: &str) -> String {
    formatdoc! {"
        fn a() {{ {a} }}

        fn b() {{ {b} }}
    "}
}

/// A repository with one committed file on `main`, a recorded working-tree review of it, and
/// `a.rs` then edited in the working tree.
fn worktree_review() -> eyre::Result<(TestRepo, Review)> {
    let repo = TestRepo::new()?;
    repo.write("a.rs", &two_functions("", ""))?;
    repo.commit("start")?;
    repo.write("a.rs", &two_functions("first();", ""))?;
    let review = Review::open(
        repo.data_dir(),
        Repo::discover(repo.path())?,
        ReviewTarget::Worktree {
            base: "main".to_owned(),
        },
    )?;
    review.refresh()?;
    Ok((repo, review))
}

/// Captures the working tree and returns the history.
fn capture(review: &Review) -> eyre::Result<History> {
    match review.live_history(&[])? {
        Capture::Captured(captured) => Ok(captured.history),
        Capture::Busy(busy) => eyre::bail!("git is busy: {busy}"),
        Capture::Moved => eyre::bail!("the repository moved during the capture"),
        Capture::Replaced => eyre::bail!("the clone was replaced"),
    }
}

/// The last revision of `history`.
fn last_revision(history: &History) -> eyre::Result<RevisionRecord> {
    let revisions = history.revisions();
    let last = revisions.last().ok_or_else(|| eyre::eyre!("no revision"))?;
    Ok((*last).clone())
}

/// The names of the refs of the review's archive, sorted.
fn refs(review: &Review) -> eyre::Result<Vec<String>> {
    let mut names: Vec<String> = review
        .snapshots()
        .refs_under("refs/")?
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    Ok(names)
}

/// The files of the review's analysis cache, sorted.
fn cached(repo: &TestRepo) -> eyre::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in walk(repo.data_dir())? {
        names.push(entry);
    }
    names.retain(|name| name.contains("analysis") || name.contains("correspond"));
    names.sort();
    Ok(names)
}

fn walk(dir: &std::path::Path) -> eyre::Result<Vec<String>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            found.extend(walk(&path)?);
        } else {
            found.push(path.display().to_string());
        }
    }
    Ok(found)
}

/// An unchanged working tree gives the recorded history; an edit gives one more, provisional
/// revision that records nothing: no event, no ref, no cache file.
#[gtest]
fn an_edit_shows_as_a_provisional_revision_that_records_nothing() -> eyre::Result<()> {
    let (repo, review) = worktree_review()?;
    let unchanged = capture(&review)?;
    expect_that!(unchanged.revisions().len(), eq(1));
    expect_false!(unchanged.is_provisional(unchanged.current().unwrap()));

    let events = std::fs::read(review.dir().events())?;
    let refs_before = refs(&review)?;
    let cache_before = cached(&repo)?;
    repo.write("a.rs", &two_functions("first();", "second();"))?;
    let live = capture(&review)?;
    let tip = live.current().unwrap();
    expect_that!(tip.get(), eq(2));
    expect_true!(live.is_provisional(tip));
    expect_true!(live.current_view()?.is_provisional());
    expect_that!(live.version(), eq(unchanged.version()));

    expect_that!(std::fs::read(review.dir().events())?, eq(&events));
    expect_that!(cached(&repo)?, eq(&cache_before));
    expect_that!(refs(&review)?, eq(&refs_before));
    expect_that!(review.history()?.revisions().len(), eq(1));
    Ok(())
}

/// A capture the window no longer shows stays readable for as long as the review is held,
/// unpinned, base and head alike: here two captures of one head against two bases.
#[gtest]
fn captures_stay_readable_while_the_review_is_held() -> eyre::Result<()> {
    let (repo, review) = worktree_review()?;
    repo.git(&["checkout", "--quiet", "-b", "feature"])?;
    repo.write("b.rs", "fn b() {}\n")?;
    repo.commit("feature")?;
    repo.write("a.rs", "fn a() { one(); }\n")?;
    let first = last_revision(&capture(&review)?)?;

    // `main` moves to the feature's tip: the capture's head is the same, its base is not.
    repo.git(&["branch", "--force", "main", "feature"])?;
    let second = last_revision(&capture(&review)?)?;
    expect_that!(second.head, eq(&first.head));
    expect_that!(second.base, not(eq(&first.base)));
    repo.write("a.rs", "fn a() { two(); }\n")?;
    capture(&review)?;

    for snapshot in [&first.base, &first.head, &second.base] {
        expect_true!(review.snapshots().tree_of(snapshot).is_ok());
    }
    Ok(())
}

/// Marking a provisional revision records it exactly as shown, then the marks; a second mark
/// made from the same history finds it recorded and adds its marks too.
#[gtest]
fn marking_a_provisional_revision_records_it_first() -> eyre::Result<()> {
    let (repo, review) = worktree_review()?;
    repo.write("a.rs", &two_functions("first();", "second();"))?;
    let live = capture(&review)?;
    let view = live.current_view()?;
    let units = summaries(&live, view.number())?;
    let first = find(&units, "first")?.id;
    let second = find(&units, "second")?.id;

    review.mark(&view, &[first], MarkVerdict::Accepted)?;
    review.mark(&view, &[second], MarkVerdict::Flagged)?;

    let recorded = review.history()?;
    expect_that!(recorded.revisions().len(), eq(2));
    expect_false!(recorded.is_provisional(view.number()));
    expect_that!(recorded.revisions()[1].head, eq(&live.revisions()[1].head));
    expect_that!(recorded.version(), gt(live.version()));
    let states = summaries(&recorded, view.number())?;
    expect_that!(
        find(&states, "first")?.state,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    expect_that!(
        find(&states, "second")?.state,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    expect_true!(refs(&review)?.contains(&review.pin_ref(&live.revisions()[1].head)));
    Ok(())
}

/// A provisional revision is never marked once the log moved on: here another revision was
/// recorded at its number meanwhile.
#[gtest]
fn a_provisional_revision_is_not_marked_once_the_log_moved_on() -> eyre::Result<()> {
    let (repo, review) = worktree_review()?;
    repo.write("a.rs", &two_functions("first();", "second();"))?;
    let live = capture(&review)?;
    let view = live.current_view()?;
    let units = view.unit_ids();
    repo.write("a.rs", "fn a() { other(); }\n")?;
    review.refresh()?;

    let refused = review.mark(&view, &units, MarkVerdict::Accepted);
    expect_that!(
        refused,
        err(matches_pattern!(Error::NotCurrentRevision { .. }))
    );
    let recorded = review.history()?;
    expect_that!(recorded.revisions().len(), eq(2));
    expect_that!(
        recorded.revisions()[1].head,
        not(eq(&live.revisions()[1].head))
    );
    Ok(())
}

/// A note on a provisional revision records it first too, so the note always refers to a
/// recorded revision.
#[gtest]
fn a_note_records_a_provisional_revision() -> eyre::Result<()> {
    let (repo, review) = worktree_review()?;
    repo.write("a.rs", &two_functions("first();", "second();"))?;
    let live = capture(&review)?;
    let view = live.current_view()?;
    let unit = find(&summaries(&live, view.number())?, "second")?.id;
    review.note(&view, unit, "why?")?;
    let recorded = review.history()?;
    expect_that!(recorded.revisions().len(), eq(2));
    expect_that!(recorded.current_view()?.notes().len(), eq(1));
    Ok(())
}

/// Marks carry from the last recorded revision to the capture along their correspondence, as
/// between recorded revisions: a change back to the marked text reads as reviewed, since only
/// recorded revisions take part in review state.
#[gtest]
fn marks_carry_to_the_capture_from_the_last_recorded_revision() -> eyre::Result<()> {
    let (repo, review) = worktree_review()?;
    let recorded = review.history()?;
    let view = recorded.current_view()?;
    review.mark(&view, &view.unit_ids(), MarkVerdict::Accepted)?;

    repo.write("a.rs", &two_functions("changed();", ""))?;
    let changed = capture(&review)?;
    let units = summaries(&changed, changed.current().unwrap())?;
    expect_that!(
        find(&units, "changed")?.state,
        matches_pattern!(UnitState::ChangedSinceReviewed { .. })
    );

    repo.write("a.rs", &two_functions("first();", ""))?;
    let back = capture(&review)?;
    expect_false!(back.is_provisional(back.current().unwrap()));
    let units = summaries(&back, back.current().unwrap())?;
    expect_that!(
        find(&units, "first")?.state,
        matches_pattern!(UnitState::Reviewed { .. })
    );
    Ok(())
}

/// A review of a range has no working tree to follow.
#[gtest]
fn only_a_working_tree_review_is_live() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("a.rs", "fn a() {}\n")?;
    repo.commit("start")?;
    repo.branch_at("feature", "main")?;
    repo.write("a.rs", "fn a() { more(); }\n")?;
    repo.commit("more")?;
    let review = repo.review()?;
    review.refresh()?;
    expect_that!(
        review.live_history(&[]).err(),
        some(matches_pattern!(Error::InvalidTarget(_)))
    );
    Ok(())
}
