//! Approving the changes of a unit that changed since it was reviewed one at a time.

use std::fmt::Write as _;

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::indoc;
use review_core::{
    Capture, Error, History, LineRun, MarkVerdict, Reopened, Repo, Review, ReviewTarget,
    ReviewedBaseline, RevisionNumber, ShownRun, SinceReviewed, UnitId, UnitState, Verdict,
};

use crate::common::{TestRepo, find, mark_all, refresh, rust_functions, summaries};

/// The base file with thirty constants appended, those `edited` given other values.
fn with_block(edited: &[(usize, &str)]) -> eyre::Result<String> {
    let mut text = rust_functions(2);
    for index in 0..30 {
        let value = edited
            .iter()
            .find(|(at, _)| *at == index)
            .map_or_else(|| index.to_string(), |(_, value)| (*value).to_owned());
        writeln!(text, "const ITEM_{index}: u32 = {value};")?;
    }
    Ok(text)
}

/// A branch that appends thirty constants, accepted at revision 1, then edits two of them far
/// apart at revision 2; returns the review and revision 2.
fn accepted_then_edited_twice() -> eyre::Result<(TestRepo, Review, RevisionNumber)> {
    let repo = TestRepo::new()?;
    repo.write("src/lib.rs", &rust_functions(2))?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write("src/lib.rs", &with_block(&[])?)?;
    repo.commit("feature")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Accepted)?;
    repo.write("src/lib.rs", &with_block(&[(5, "500"), (25, "2500")])?)?;
    repo.commit("two edits")?;
    let second = refresh(&review)?;
    Ok((repo, review, second))
}

/// The one unit of `revision`, the appended block.
fn block_unit(review: &Review, revision: RevisionNumber) -> eyre::Result<UnitId> {
    let history = review.history()?;
    Ok(find(&summaries(&history, revision)?, "ITEM_0")?.id)
}

/// The changes `unit` shows on top of what was reviewed, as their current lines.
fn runs(review: &Review, unit: UnitId) -> eyre::Result<Vec<String>> {
    let history = review.history()?;
    let view = history.view(unit.revision())?;
    match view.since_reviewed(unit)? {
        Some(SinceReviewed::Delta(delta)) => Ok(delta
            .runs()
            .iter()
            .map(|run| run.added.to_owned())
            .collect()),
        other => Err(eyre::eyre!("expected a delta, got {other:?}")),
    }
}

/// Change `run` of `unit` as a view of `history` shows it.
fn shown(history: &History, unit: UnitId, run: usize) -> eyre::Result<ShownRun> {
    match history.view(unit.revision())?.since_reviewed(unit)? {
        Some(SinceReviewed::Delta(delta)) => delta
            .runs()
            .get(run)
            .map(LineRun::shown)
            .ok_or_else(|| eyre::eyre!("no change {run}")),
        other => Err(eyre::eyre!("expected a delta, got {other:?}")),
    }
}

/// Approves change `run` of `unit` as a view read now shows it.
fn approve(review: &Review, unit: UnitId, run: usize) -> eyre::Result<()> {
    let history = review.history()?;
    let view = history.view(unit.revision())?;
    review.approve_run(&view, &shown(&history, unit, run)?)?;
    Ok(())
}

/// Whether `unit` is reviewed as a whole, in a history read now.
fn reviewed(review: &Review, unit: UnitId) -> eyre::Result<bool> {
    let history = review.history()?;
    Ok(history.view(unit.revision())?.state(unit)?.is_reviewed())
}

/// Two distant edits to an accepted addition are two changes to approve: approving the first
/// leaves the second, still against the reviewed text, and the unit open; approving the last
/// accepts the unit with an ordinary mark.
#[test]
fn distant_edits_are_approved_one_at_a_time() -> eyre::Result<()> {
    let (_repo, review, second) = accepted_then_edited_twice()?;
    let unit = block_unit(&review, second)?;
    assert_that!(
        runs(&review, unit)?,
        elements_are![
            eq("const ITEM_5: u32 = 500;\n"),
            eq("const ITEM_25: u32 = 2500;\n")
        ]
    );

    approve(&review, unit, 0)?;
    assert!(!reviewed(&review, unit)?);
    // The unit stays open, counted as changed, and only the second edit shows.
    let history = review.history()?;
    let view = history.view(second)?;
    assert_that!(
        view.state(unit)?,
        eq(UnitState::ChangedSinceReviewed {
            verdict: Verdict::Accepted,
            reviewed: unit
        })
    );
    assert_that!(view.status().total.changed, eq(1));
    assert_that!(
        runs(&review, unit)?,
        elements_are![eq("const ITEM_25: u32 = 2500;\n")]
    );
    // The approved text is in no file, so its lines have no head to be colored from.
    let Some(SinceReviewed::Delta(delta)) = view.since_reviewed(unit)? else {
        return Err(eyre::eyre!("expected a delta"));
    };
    assert_that!(delta.baseline, eq(ReviewedBaseline::Partial));
    assert!(delta.before.contains("const ITEM_5: u32 = 500;\n"));
    assert!(delta.before.contains("const ITEM_25: u32 = 25;\n"));

    approve(&review, unit, 0)?;
    assert!(reviewed(&review, unit)?);
    let history = review.history()?;
    let view = history.view(second)?;
    assert_that!(
        view.state(unit)?,
        eq(UnitState::Reviewed {
            verdict: Verdict::Accepted,
            marked: unit,
            inherited: false
        })
    );
    assert_that!(view.status().total.changed, eq(0));
    // One partial approval, then an ordinary accepted mark.
    let log = std::fs::read_to_string(review.dir().events())?;
    let tags: Vec<&str> = log
        .lines()
        .filter_map(|line| line.split('"').nth(3))
        .collect();
    assert_that!(
        tags,
        elements_are![
            eq(&"revision"),
            eq(&"mark"),
            eq(&"revision"),
            eq(&"partial_approval"),
            eq(&"mark")
        ]
    );
    Ok(())
}

/// Approving a deletion removes exactly its reviewed lines, among repeated lines and after an
/// insertion that shifts the current lines, and a change of only a line ending or the final
/// newline stays to approve on its own, from a view read before an insertion above it was
/// approved too.
#[test]
fn approving_splices_exactly_the_approved_bytes() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("README.md", "readme\n")?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let kept = indoc! {"
        one
        two
        x
        x
        x
        four
        five
        six
        seven
        eight
        nine
    "};
    repo.write("notes.txt", &[kept, "tail\n", "tail\n"].concat())?;
    let ending = indoc! {"
        a
        b
        c
        d
        e
        f
        g
        h
    "};
    repo.write("ending.txt", ending)?;
    repo.commit("feature")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Accepted)?;
    // A line inserted after the first, a CRLF on `four` and both tails deleted.
    let notes_now = kept
        .replacen("one\n", &["one\n", "new\n"].concat(), 1)
        .replace("four\n", "four\r\n");
    repo.write("notes.txt", &notes_now)?;
    // `b` edited and the final newline dropped.
    repo.write("ending.txt", ending.replace("b\n", "B\n").trim_end())?;
    repo.commit("edits")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    let notes = find(&units, "four")?.id;
    let endings = find(&units, "B\n")?.id;
    drop(history);

    // An insertion, a line-ending change and a deletion at the end; the deletion goes first.
    assert_that!(
        runs(&review, notes)?,
        elements_are![eq("new\n"), eq("four\r\n"), eq("")]
    );
    approve(&review, notes, 2)?;
    assert!(!reviewed(&review, notes)?);
    let history = review.history()?;
    let view = history.view(second)?;
    let Some(SinceReviewed::Delta(delta)) = view.since_reviewed(notes)? else {
        return Err(eyre::eyre!("expected a delta"));
    };
    assert_that!(delta.before, eq(kept));
    let left = delta.runs();
    assert_that!(left.len(), eq(2));
    assert_that!(left[1].removed, eq("four\n"));
    assert_that!(left[1].added, eq("four\r\n"));

    // The CRLF change is a change of its own. Shown before the insertion above it is
    // approved, it is still the same change after, though its reviewed line moved down by one,
    // and approving it accepts the unit.
    let stale = review.history()?;
    let crlf = shown(&stale, notes, 1)?;
    approve(&review, notes, 0)?;
    assert_that!(runs(&review, notes)?, elements_are![eq("four\r\n")]);
    review.approve_run(&stale.view(second)?, &crlf)?;
    assert!(reviewed(&review, notes)?);

    // So is the dropped final newline.
    assert_that!(runs(&review, endings)?, elements_are![eq("B\n"), eq("h")]);
    approve(&review, endings, 1)?;
    assert!(!reviewed(&review, endings)?);
    let history = review.history()?;
    let view = history.view(second)?;
    let Some(SinceReviewed::Delta(delta)) = view.since_reviewed(endings)? else {
        return Err(eyre::eyre!("expected a delta"));
    };
    assert_that!(delta.before, eq(ending.trim_end()));
    approve(&review, endings, 0)?;
    assert!(reviewed(&review, endings)?);
    Ok(())
}

/// A press survives what other presses and windows record meanwhile: a change shown before
/// another change of its unit was approved, and before another unit was marked, is approved.
#[test]
fn an_approval_survives_other_records() -> eyre::Result<()> {
    let (_repo, review, second) = accepted_then_edited_twice()?;
    let unit = block_unit(&review, second)?;
    let stale = review.history()?;
    let later = shown(&stale, unit, 1)?;
    approve(&review, unit, 0)?;
    review.approve_run(&stale.view(second)?, &later)?;
    assert!(reviewed(&review, unit)?);
    Ok(())
}

/// A change approved or changed since it was shown is refused, and nothing is recorded.
#[test]
fn an_approval_of_a_change_no_longer_shown_is_refused() -> eyre::Result<()> {
    let (_repo, review, second) = accepted_then_edited_twice()?;
    let unit = block_unit(&review, second)?;
    let stale = review.history()?;
    let first = shown(&stale, unit, 0)?;
    approve(&review, unit, 0)?;

    let refused = review.approve_run(&stale.view(second)?, &first);
    assert_that!(
        refused,
        err(matches_pattern!(Error::ChangedSinceShown {
            unit: eq(&unit)
        }))
    );
    // Nothing more was recorded: the second edit still shows.
    assert_that!(
        runs(&review, unit)?,
        elements_are![eq("const ITEM_25: u32 = 2500;\n")]
    );
    // Nor is a change whose current lines differ from what showed.
    let changed = ShownRun {
        added: "const ITEM_25: u32 = 2501;\n".to_owned(),
        ..shown(&review.history()?, unit, 0)?
    };
    let history = review.history()?;
    assert_that!(
        review.approve_run(&history.view(second)?, &changed),
        err(matches_pattern!(Error::ChangedSinceShown { .. }))
    );
    // Nor one at the same place whose reviewed lines differ from those it replaces now.
    let replaced = ShownRun {
        removed: "const ITEM_25: u32 = 24;\n".to_owned(),
        ..shown(&review.history()?, unit, 0)?
    };
    assert_that!(
        review.approve_run(&history.view(second)?, &replaced),
        err(matches_pattern!(Error::ChangedSinceShown { .. }))
    );
    assert!(!reviewed(&review, unit)?);
    Ok(())
}

/// A working tree accepted at revision 1 and edited twice since, captured but not recorded,
/// with the capture's history.
fn captured_edits(repo: &TestRepo) -> eyre::Result<(Review, History)> {
    repo.write("src/lib.rs", &rust_functions(2))?;
    repo.commit("start")?;
    repo.write("src/lib.rs", &with_block(&[])?)?;
    let review = Review::open(
        repo.data_dir(),
        Repo::discover(repo.path())?,
        ReviewTarget::Worktree {
            base: "main".to_owned(),
        },
    )?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Accepted)?;
    repo.write("src/lib.rs", &with_block(&[(5, "500"), (25, "2500")])?)?;
    let session = review.start_session()?;
    let captured = match review.live_history(&session, &[], &[])? {
        Capture::Captured(captured) => captured.history,
        _ => eyre::bail!("the working tree was not captured"),
    };
    Ok((review, captured))
}

/// An approval the log refuses records nothing, not even the capture it was made in: another
/// window reset what the capture's change was compared with, so the change no longer shows,
/// and the capture stays unrecorded.
#[test]
fn a_refused_approval_records_no_capture() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let (review, captured) = captured_edits(&repo)?;
    let view = captured.current_view()?;
    assert!(view.is_provisional());
    let unit = view.unit_ids()[0];
    let change = shown(&captured, unit, 0)?;
    mark_all(&review, RevisionNumber::FIRST, MarkVerdict::Unseen)?;

    assert_that!(
        review.approve_run(&view, &change),
        err(matches_pattern!(Error::ChangedSinceShown { .. }))
    );
    assert_that!(review.history()?.revisions().len(), eq(1));
    Ok(())
}

/// A partial approval carries along an unchanged link with what is still open; a later edit of
/// the approved place compares with the approved text, and a head that returns to it after a
/// changed link stays open until a mark.
#[test]
fn a_partial_approval_carries_and_never_approves_by_equality() -> eyre::Result<()> {
    let (repo, review, second) = accepted_then_edited_twice()?;
    let unit = block_unit(&review, second)?;
    approve(&review, unit, 0)?;

    // An unrelated file changes: the second edit is still the only change to approve.
    repo.write("other.rs", "fn other() {}\n")?;
    repo.commit("unrelated")?;
    let third = refresh(&review)?;
    let carried = block_unit(&review, third)?;
    assert_that!(
        runs(&review, carried)?,
        elements_are![eq("const ITEM_25: u32 = 2500;\n")]
    );

    // The approved place changes again: it shows against the approved `500`.
    repo.write("src/lib.rs", &with_block(&[(5, "501"), (25, "2500")])?)?;
    repo.commit("edit the approved place")?;
    let fourth = refresh(&review)?;
    let edited = block_unit(&review, fourth)?;
    let history = review.history()?;
    let view = history.view(fourth)?;
    let Some(SinceReviewed::Delta(delta)) = view.since_reviewed(edited)? else {
        return Err(eyre::eyre!("expected a delta"));
    };
    let edits: Vec<(&str, &str)> = delta
        .runs()
        .iter()
        .map(|run| (run.removed, run.added))
        .collect();
    assert_that!(
        edits,
        elements_are![
            eq(&("const ITEM_5: u32 = 500;\n", "const ITEM_5: u32 = 501;\n")),
            eq(&("const ITEM_25: u32 = 25;\n", "const ITEM_25: u32 = 2500;\n"))
        ]
    );
    drop(history);

    // The head returns to exactly the approved text: it shows whole and stays open.
    repo.write("src/lib.rs", &with_block(&[(5, "500")])?)?;
    repo.commit("back to the approved text")?;
    let fifth = refresh(&review)?;
    let reverted = block_unit(&review, fifth)?;
    let history = review.history()?;
    let view = history.view(fifth)?;
    assert_that!(
        view.since_reviewed(reverted)?,
        some(eq(SinceReviewed::Whole(Reopened::SameCode)))
    );
    assert!(!view.state(reverted)?.is_reviewed());
    Ok(())
}

/// A partial approval carries no comparison across a link that is not trusted: the unit shows
/// whole and offers no change to approve on its own.
#[test]
fn an_untrusted_link_after_a_partial_approval_shows_whole() -> eyre::Result<()> {
    let (repo, review, second) = accepted_then_edited_twice()?;
    let unit = block_unit(&review, second)?;
    approve(&review, unit, 0)?;

    // A line just above the block changes, in its context: the block's own change is the same,
    // but its surroundings are not.
    let mut edited = with_block(&[(5, "500"), (25, "2500")])?;
    let last = edited
        .rfind("    value\n")
        .ok_or_else(|| eyre::eyre!("no function before the block"))?;
    edited.replace_range(last..last + "    value".len(), "    value + 0");
    repo.write("src/lib.rs", &edited)?;
    repo.commit("edit next to the block")?;
    let third = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, third)?;
    let block = find(&units, "ITEM_0")?.id;
    let view = history.view(third)?;
    assert_that!(
        view.since_reviewed(block)?,
        some(eq(SinceReviewed::Whole(Reopened::Uncertain)))
    );
    // The change as revision 2 showed it is no change this revision shows.
    let earlier = ShownRun {
        unit: block,
        ..shown(&history, unit, 0)?
    };
    assert_that!(
        review.approve_run(&view, &earlier),
        err(matches_pattern!(Error::ChangedSinceShown { .. }))
    );
    Ok(())
}

/// A whole mark after a partial approval replaces it: a flag shows the unit whole once it
/// changes again, a reset makes it new, and an acceptance compares later changes with the
/// head it accepted, not with the partially approved text.
#[test]
fn a_whole_mark_supersedes_a_partial_approval() -> eyre::Result<()> {
    for (verdict, expected) in [
        (
            MarkVerdict::Flagged,
            Some(SinceReviewed::Whole(Reopened::Flagged)),
        ),
        (MarkVerdict::Unseen, None),
    ] {
        let (repo, review, second) = accepted_then_edited_twice()?;
        let unit = block_unit(&review, second)?;
        approve(&review, unit, 0)?;
        let history = review.history()?;
        review.mark(&history.view(second)?, &[unit], verdict)?;
        repo.write("src/lib.rs", &with_block(&[(5, "500"), (25, "2501")])?)?;
        repo.commit("edit again")?;
        let third = refresh(&review)?;
        let history = review.history()?;
        let view = history.view(third)?;
        let block = find(&summaries(&history, third)?, "ITEM_0")?.id;
        assert_that!(view.since_reviewed(block)?, eq(expected), "{verdict:?}");
    }

    let (repo, review, second) = accepted_then_edited_twice()?;
    let unit = block_unit(&review, second)?;
    approve(&review, unit, 0)?;
    let history = review.history()?;
    review.mark(&history.view(second)?, &[unit], MarkVerdict::Accepted)?;
    repo.write("src/lib.rs", &with_block(&[(5, "500"), (25, "2501")])?)?;
    repo.commit("edit again")?;
    let third = refresh(&review)?;
    let block = block_unit(&review, third)?;
    let history = review.history()?;
    let view = history.view(third)?;
    let Some(SinceReviewed::Delta(delta)) = view.since_reviewed(block)? else {
        return Err(eyre::eyre!("expected a delta"));
    };
    assert_that!(delta.reviewed, eq(unit));
    assert!(matches!(delta.baseline, ReviewedBaseline::Head(_)));
    let edits: Vec<&str> = delta.runs().iter().map(|run| run.removed).collect();
    assert_that!(edits, elements_are![eq(&"const ITEM_25: u32 = 2500;\n")]);
    Ok(())
}

/// Two captures of a working tree that show different edits as the same provisional revision
/// share a log version: approving a change of one records that capture, and the other's
/// approval is refused rather than applied to a capture it never showed.
#[test]
fn an_approval_never_lands_on_another_capture() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    repo.write("src/lib.rs", &rust_functions(2))?;
    repo.commit("start")?;
    repo.write("src/lib.rs", &with_block(&[])?)?;
    let review = Review::open(
        repo.data_dir(),
        Repo::discover(repo.path())?,
        ReviewTarget::Worktree {
            base: "main".to_owned(),
        },
    )?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Accepted)?;
    let session = review.start_session()?;
    let capture = || -> eyre::Result<History> {
        match review.live_history(&session, &[], &[])? {
            Capture::Captured(captured) => Ok(captured.history),
            _ => eyre::bail!("the working tree was not captured"),
        }
    };
    repo.write("src/lib.rs", &with_block(&[(5, "500"), (25, "2500")])?)?;
    let seen = capture()?;
    repo.write("src/lib.rs", &with_block(&[(6, "600"), (26, "2600")])?)?;
    let other = capture()?;
    assert_that!(seen.version(), eq(other.version()));
    let (shown_view, other_view) = (seen.current_view()?, other.current_view()?);
    assert_that!(shown_view.number(), eq(other_view.number()));
    let unit = shown_view.unit_ids()[0];

    review.approve_run(&shown_view, &shown(&seen, unit, 0)?)?;
    let other_unit = other_view.unit_ids()[0];
    assert_that!(
        review.approve_run(&other_view, &shown(&other, other_unit, 0)?),
        err(matches_pattern!(Error::NotCurrentRevision { .. }))
    );
    // The recorded revision is the capture the approval was made in.
    let history = review.history()?;
    let recorded = history.view(shown_view.number())?;
    assert_that!(
        recorded.record().map(|record| &record.head),
        eq(shown_view.record().map(|record| &record.head))
    );
    Ok(())
}
