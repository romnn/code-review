//! Walkthrough import, validation and carrying across revisions.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::walkthrough::{StepKind, partition};

use crate::common::{TestRepo, find, refresh, rust_functions, summaries};

fn walkthrough_json(review: &str, revision: u32, steps: &[(&str, Vec<String>)]) -> String {
    let steps: Vec<serde_json::Value> = steps
        .iter()
        .map(|(title, units)| serde_json::json!({"title": title, "why": "because", "units": units}))
        .collect();
    serde_json::json!({
        "version": 1,
        "review": review,
        "revision": revision,
        "title": "Tour",
        "steps": steps,
    })
    .to_string()
}

/// A walkthrough survives a new revision: units keep their steps, new units go to "New since
/// walkthrough", and stale references are dropped with a warning.
#[test]
fn walkthrough_survives_new_revision() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(10);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let first_text = base
        .replace("let value = 1;", "let value = 10;")
        .replace("let value = 4;", "let value = 40;")
        .replace("let value = 7;", "let value = 70;");
    repo.write("src/lib.rs", &first_text)?;
    repo.commit("one")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, first)?;
    let [a, b, c] =
        ["10", "40", "70"].map(|needle| find(&units, needle).map(|unit| unit.id.to_string()));
    let (a, b, c) = (a?, b?, c?);
    let key = review.key().to_string();
    let json = walkthrough_json(
        &key,
        first.get(),
        &[
            ("Core", vec![b.clone()]),
            ("Rest", vec![a.clone(), c.clone()]),
        ],
    );
    review.import_walkthrough(&history, &json)?;

    // Amend A, drop C, add D.
    let second_text = base
        .replace("let value = 1;", "let value = 11;")
        .replace("let value = 4;", "let value = 40;")
        .replace("let value = 9;", "let value = 90;");
    repo.write("src/lib.rs", &second_text)?;
    repo.commit("two")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let view = history.view(second)?;
    let units = summaries(&history, second)?;
    let parts = partition(&view);
    let titles: Vec<(&str, StepKind)> = parts
        .steps
        .iter()
        .map(|step| (step.title.as_str(), step.kind))
        .collect();
    assert_that!(
        titles,
        elements_are![
            eq(&("Core", StepKind::Authored)),
            eq(&("Rest", StepKind::Authored)),
            eq(&("New since walkthrough", StepKind::NewSinceWalkthrough))
        ]
    );
    assert_that!(
        parts.steps[0].units,
        elements_are![eq(&find(&units, "40")?.id)]
    );
    assert_that!(
        parts.steps[1].units,
        elements_are![eq(&find(&units, "11")?.id)]
    );
    assert_that!(
        parts.steps[2].units,
        elements_are![eq(&find(&units, "90")?.id)]
    );
    assert!(
        parts.warnings.iter().any(|warning| warning.contains(&c)),
        "{:?}",
        parts.warnings
    );

    // Every unit is in exactly one step.
    let mut all: Vec<_> = parts
        .steps
        .iter()
        .flat_map(|step| step.units.clone())
        .collect();
    all.sort();
    assert_that!(all, eq(&view.unit_ids()));
    Ok(())
}

/// Import rejects a walkthrough for another review or revision, and unknown or duplicate IDs.
#[test]
fn invalid_walkthroughs_are_rejected() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(4);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 10;"),
    )?;
    repo.commit("one")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    let history = review.history()?;
    let key = review.key().to_string();
    let unit = history.view(first)?.unit_ids()[0].to_string();
    let attempts = [
        walkthrough_json("github.com/other/repo#1", 1, &[("A", vec![unit.clone()])]),
        walkthrough_json(&key, 2, &[("A", vec!["u2-0000".to_owned()])]),
        walkthrough_json(&key, 1, &[("A", vec!["u1-0099".to_owned()])]),
        walkthrough_json(
            &key,
            1,
            &[("A", vec![unit.clone()]), ("B", vec![unit.clone()])],
        ),
        walkthrough_json(&key, 1, &[("A", vec!["u2-0000".to_owned()])]),
        walkthrough_json(&key, 1, &[("A", vec!["0000".to_owned()])]),
        "{not json".to_owned(),
    ];
    for attempt in attempts {
        let result = review.import_walkthrough(&history, &attempt);
        assert!(
            matches!(result, Err(review_core::Error::Walkthrough(_))),
            "{attempt}: {result:?}"
        );
    }
    review.import_walkthrough(&history, &walkthrough_json(&key, 1, &[("A", vec![unit])]))?;
    Ok(())
}

/// A walkthrough written for A carries through B and a force-push back to A (revisions 1, 2
/// and 3): units kept on every revision keep their steps, and a unit B dropped comes back as new.
#[test]
fn walkthrough_carries_through_a_force_push_back() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(10);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let text_a = base
        .replace("let value = 1;", "let value = 10;")
        .replace("let value = 5;", "let value = 50;")
        .replace("let value = 8;", "let value = 80;");
    repo.write("src/lib.rs", &text_a)?;
    let head_a = repo.commit("A")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, first)?;
    let [x, y, z] =
        ["10", "50", "80"].map(|needle| find(&units, needle).map(|unit| unit.id.to_string()));
    let key = review.key().to_string();
    let json = walkthrough_json(
        &key,
        first.get(),
        &[("First", vec![x?]), ("Second", vec![y?, z?])],
    );
    review.import_walkthrough(&history, &json)?;

    // B drops the edit in f8.
    repo.write(
        "src/lib.rs",
        &text_a.replace("let value = 80;", "let value = 8;"),
    )?;
    repo.commit("B")?;
    refresh(&review)?;
    repo.branch_at("feature", &head_a)?;
    let third = refresh(&review)?;
    assert_that!(third.get(), eq(3));

    let history = review.history()?;
    let view = history.view(third)?;
    let units = summaries(&history, third)?;
    let parts = partition(&view);
    let steps: Vec<(&str, Vec<_>)> = parts
        .steps
        .iter()
        .map(|step| (step.title.as_str(), step.units.clone()))
        .collect();
    assert_that!(
        steps,
        elements_are![
            eq(&("First", vec![find(&units, "10")?.id])),
            eq(&("Second", vec![find(&units, "50")?.id])),
            eq(&("New since walkthrough", vec![find(&units, "80")?.id]))
        ]
    );
    Ok(())
}

/// Front ends navigate through the engine: review order follows the partition, neighbors clamp
/// at the ends, "next unreviewed" skips reviewed units and wraps, and step progress counts marks.
#[test]
fn navigation_and_progress_follow_the_partition() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(20);
    repo.write("src/a.rs", &base)?;
    repo.write("src/b.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let edited = base
        .replace("let value = 2;", "let value = 20;")
        .replace("let value = 12;", "let value = 120;");
    repo.write("src/a.rs", &edited)?;
    repo.write("src/b.rs", &edited)?;
    repo.commit("edit")?;
    let review = repo.review()?;
    let revision = refresh(&review)?;
    let history = review.history()?;
    let view = history.view(revision)?;
    let parts = partition(&view);
    let order = parts.order();
    assert_that!(order.len(), eq(4));
    assert_that!(parts.step_of(order[2]), some(eq(1)));
    assert_that!(parts.neighbor(None, 1), some(eq(order[0])));
    assert_that!(parts.neighbor(Some(order[0]), -1), some(eq(order[0])));
    assert_that!(parts.neighbor(Some(order[2]), 5), some(eq(order[3])));

    // Mark the first two; the next unreviewed after the last unit wraps to the third.
    review.mark(&view, &order[..2], review_core::MarkVerdict::Seen)?;
    let history = review.history()?;
    let view = history.view(revision)?;
    let parts = partition(&view);
    assert_that!(
        parts.next_unreviewed(&view, Some(order[3])),
        some(eq(order[2]))
    );
    assert_that!(parts.next_unreviewed(&view, None), some(eq(order[2])));
    let progress: Vec<(usize, usize)> = parts
        .progress(&view)
        .iter()
        .map(|step| (step.reviewed, step.total))
        .collect();
    assert_that!(progress, elements_are![eq(&(2, 2)), eq(&(0, 2))]);
    review.mark(&view, &order[2..], review_core::MarkVerdict::Accepted)?;
    let history = review.history()?;
    let view = history.view(revision)?;
    assert_that!(partition(&view).next_unreviewed(&view, None), none());
    Ok(())
}

/// Targets come from command-line style arguments in one place for every front end, and the
/// review opened last for a branch is remembered.
#[test]
fn targets_parse_from_arguments_and_reviews_are_remembered() -> eyre::Result<()> {
    use review_core::{Repo, ReviewTarget};

    let repo = TestRepo::new()?;
    repo.write("a.txt", "a\n")?;
    repo.commit("base")?;
    let git = Repo::discover(repo.path())?;
    assert_that!(
        ReviewTarget::from_arguments(Some("main..feature"), None, &git)?,
        eq(&ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned()
        })
    );
    assert_that!(
        ReviewTarget::from_arguments(Some("main..feature"), Some("HEAD"), &git)?,
        eq(&ReviewTarget::Worktree {
            base: "HEAD".to_owned()
        })
    );
    assert!(ReviewTarget::from_arguments(None, None, &git).is_err());
    assert!(
        ReviewTarget::from_arguments(Some("https://example.com/not-a-pull"), None, &git).is_err()
    );

    assert_that!(
        review_core::store::remembered_review(repo.data_dir(), &git)?,
        none()
    );
    let review = repo.review()?;
    review_core::store::remember_review(repo.data_dir(), &git, review.key())?;
    assert_that!(
        review_core::store::remembered_review(repo.data_dir(), &git)?,
        some(eq(review.key()))
    );
    assert_that!(
        review_core::store::default_data_dir(
            Some(std::path::Path::new("")),
            Some(std::path::Path::new("/home/u"))
        ),
        some(eq(std::path::Path::new("/home/u/.local/share/code-review")))
    );
    Ok(())
}
