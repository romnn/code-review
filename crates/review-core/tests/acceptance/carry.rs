//! Carrying review state across revisions.

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::{formatdoc, indoc};
use review_core::{DiffLineKind, LineEnding, MarkVerdict, NoteAttachment, UnitState, Verdict};

use crate::common::{TestRepo, find, mark_all, refresh, rust_functions, summaries};

fn is_reviewed(state: UnitState) -> bool {
    state.is_reviewed()
}

/// Marks everything, amends one chunk, rebases onto a newer base and force-updates the head:
/// only the amended chunk is unreviewed, and it is changed since reviewed.
#[test]
fn force_push_carry_over() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(10);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let edit = |text: &str, fifth: &str| {
        text.replace("let value = 1;", "let value = 100;")
            .replace("let value = 5;", fifth)
            .replace("let value = 8;", "let value = 800;")
    };
    repo.write("src/lib.rs", &edit(&base, "let value = 500;"))?;
    repo.commit("feature")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    // The target branch moves on, and the head is rebased onto it with one chunk amended.
    repo.switch("main")?;
    let newer_base = formatdoc!(
        "
            fn top() {{}}

            {base}"
    );
    repo.write("src/lib.rs", &newer_base)?;
    repo.write("README.md", "unrelated\n")?;
    repo.commit("newer base")?;
    repo.branch_at("feature", "main")?;
    repo.write("src/lib.rs", &edit(&newer_base, "let value = 555;"))?;
    repo.commit("rebased and amended")?;
    let second = refresh(&review)?;
    assert_that!(second.get(), eq(2));

    let history = review.history()?;
    let units = summaries(&history, second)?;
    assert_that!(units.len(), eq(3));
    assert!(is_reviewed(find(&units, "100")?.state));
    assert!(is_reviewed(find(&units, "800")?.state));
    let amended = find(&units, "555")?;
    let first_units = summaries(&history, first)?;
    let reviewed = find(&first_units, "500")?.id;
    assert_that!(
        amended.state,
        eq(UnitState::ChangedSinceReviewed {
            verdict: Verdict::Seen,
            reviewed
        })
    );
    let status = history.view(second)?.status();
    assert_that!(status.total.unreviewed(), eq(1));
    assert_that!(status.total.changed, eq(1));
    Ok(())
}

/// An unrelated edit above a reviewed chunk does not reopen it.
#[test]
fn line_shift_keeps_review() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(10);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let edited = base.replace("let value = 7;", "let value = 70;");
    repo.write("src/lib.rs", &edited)?;
    repo.commit("edit f7")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    repo.write(
        "src/lib.rs",
        &formatdoc!(
            "
                // header
                // more
                {}",
            edited.replace("let value = 0;", "let value = 1000;")
        ),
    )?;
    repo.commit("edit above")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    assert!(is_reviewed(find(&units, "let value = 70;")?.state));
    assert_that!(find(&units, "1000")?.state, eq(UnitState::New));
    assert_that!(find(&units, "// header")?.state, eq(UnitState::New));
    Ok(())
}

fn logging_functions(count: usize) -> String {
    (0..count)
        .map(|index| {
            formatdoc!(
                "
                    fn f{index}() -> u32 {{
                        log(\"enter\");
                        let value = {index};
                        value
                    }}

                "
            )
        })
        .collect::<Vec<_>>()
        .concat()
}

/// An identical edit added earlier in the file is new, and the reviewed one stays reviewed.
#[test]
fn duplicate_edit_is_new() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = logging_functions(10);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let in_f7 = base.replace(
        indoc! {"
            \x20   log(\"enter\");
                let value = 7;"},
        indoc! {"
            \x20   trace(\"enter\");
                let value = 7;"},
    );
    repo.write("src/lib.rs", &in_f7)?;
    repo.commit("edit f7")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    let also_f2 = in_f7.replace(
        indoc! {"
            \x20   log(\"enter\");
                let value = 2;"},
        indoc! {"
            \x20   trace(\"enter\");
                let value = 2;"},
    );
    repo.write("src/lib.rs", &also_f2)?;
    repo.commit("same edit in f2")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let view = history.view(second)?;
    let units = summaries(&history, second)?;
    assert_that!(units.len(), eq(2));
    // Units are in position order: the copy in f2 comes first.
    let [copy, original] = units.as_slice() else {
        return Err(eyre::eyre!("expected two units"));
    };
    assert_that!(
        view.location(copy.id)?.line,
        lt(view.location(original.id)?.line)
    );
    assert_that!(copy.state, eq(UnitState::New));
    assert!(is_reviewed(original.state));
    Ok(())
}

/// Reviewed at revision 1, unreviewed changes at 2, more at 3: revision 3 shows the interdiff
/// against revision 1.
#[test]
fn skipped_revision_interdiff() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(6);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let with = |fourth: &str| {
        base.replace("let value = 1;", "let value = 100;")
            .replace("let value = 4;", fourth)
    };
    repo.write("src/lib.rs", &with("let value = 400;"))?;
    repo.commit("one")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;
    repo.write("src/lib.rs", &with("let value = 401;"))?;
    repo.commit("two")?;
    refresh(&review)?;
    repo.write("src/lib.rs", &with("let value = 402;"))?;
    repo.commit("three")?;
    let third = refresh(&review)?;
    assert_that!(third.get(), eq(3));

    let history = review.history()?;
    let units = summaries(&history, third)?;
    assert!(is_reviewed(find(&units, "100")?.state));
    let changed = find(&units, "402")?;
    let UnitState::ChangedSinceReviewed { reviewed, .. } = changed.state else {
        return Err(eyre::eyre!(
            "expected changed since reviewed, got {:?}",
            changed.state
        ));
    };
    assert_that!(reviewed.revision(), eq(first));
    let interdiff = history
        .view(third)?
        .interdiff(changed.id)?
        .ok_or_else(|| eyre::eyre!("no interdiff"))?;
    assert!(!interdiff.base.changed);
    assert!(interdiff.head.changed);
    let lines: Vec<(DiffLineKind, &str)> = interdiff
        .head
        .lines
        .iter()
        .map(|line| (line.kind, line.text.as_str()))
        .collect();
    assert_that!(
        lines,
        unordered_elements_are![
            eq(&(DiffLineKind::Old, "    let value = 400;")),
            eq(&(DiffLineKind::New, "    let value = 402;"))
        ]
    );
    Ok(())
}

/// The head stays while the target branch changes the removed side of a reviewed chunk: the
/// chunk is changed since reviewed, and the interdiff shows the removed side.
#[test]
fn base_only_change_reopens() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(4);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 2;", "let value = 222;"),
    )?;
    repo.commit("feature")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Accepted)?;

    repo.switch("main")?;
    let newer_base = base.replace("let value = 2;", "let value = 20;");
    repo.write("src/lib.rs", &newer_base)?;
    repo.commit("target changes the line")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 2;", "let value = 222;"),
    )?;
    repo.commit("rebased, same head content")?;
    let second = refresh(&review)?;

    let history = review.history()?;
    let units = summaries(&history, second)?;
    let unit = find(&units, "222")?;
    assert!(matches!(
        unit.state,
        UnitState::ChangedSinceReviewed {
            verdict: Verdict::Accepted,
            ..
        }
    ));
    let interdiff = history
        .view(second)?
        .interdiff(unit.id)?
        .ok_or_else(|| eyre::eyre!("no interdiff"))?;
    assert!(interdiff.base.changed);
    assert!(!interdiff.head.changed);
    assert!(
        interdiff
            .base
            .lines
            .iter()
            .any(|line| line.kind == DiffLineKind::Old && line.text.contains("value = 2;"))
    );
    assert!(
        interdiff
            .base
            .lines
            .iter()
            .any(|line| line.kind == DiffLineKind::New && line.text.contains("value = 20;"))
    );
    Ok(())
}

/// Deletion-only chunks carry over, and reopen when the deletion changes.
#[test]
fn deletion_only_chunks_carry_and_reopen() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(8);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let deleted = base.replace(
        indoc! {"
            \x20   let value = 3;
                value
        "},
        "",
    );
    repo.write("src/lib.rs", &deleted)?;
    repo.commit("delete two lines")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    repo.write(
        "src/lib.rs",
        &deleted.replace("let value = 6;", "let value = 60;"),
    )?;
    repo.commit("unrelated edit")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    assert!(is_reviewed(find(&units, "let value = 3;")?.state));

    let changed_deletion = base
        .replace("    let value = 3;\n", "")
        .replace("let value = 6;", "let value = 60;");
    repo.write("src/lib.rs", &changed_deletion)?;
    repo.commit("delete one line only")?;
    let third = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, third)?;
    let deletion = find(&units, "let value = 3;")?;
    assert!(
        matches!(deletion.state, UnitState::ChangedSinceReviewed { .. }),
        "{:?}",
        deletion.state
    );
    Ok(())
}

/// Two identical files with identical edits, renamed in a rebase, reopen.
#[test]
fn ambiguous_renames_reopen() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(3);
    repo.write("x.rs", &base)?;
    repo.write("y.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let edited = base.replace("let value = 1;", "let value = 100;");
    repo.write("x.rs", &edited)?;
    repo.write("y.rs", &edited)?;
    repo.commit("edit both")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    repo.switch("main")?;
    repo.git(&["mv", "x.rs", "x2.rs"])?;
    repo.git(&["mv", "y.rs", "y2.rs"])?;
    repo.commit("rename both")?;
    repo.branch_at("feature", "main")?;
    repo.write("x2.rs", &edited)?;
    repo.write("y2.rs", &edited)?;
    repo.commit("rebased")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    assert_that!(units.len(), eq(2));
    assert!(
        units.iter().all(|unit| !is_reviewed(unit.state)),
        "{units:#?}"
    );
    Ok(())
}

/// The control for [`ambiguous_renames_reopen`]: one file renamed by the target branch keeps its
/// reviewed chunks.
#[test]
fn unambiguous_base_rename_keeps_review() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(3);
    repo.write("x.rs", &base)?;
    repo.write("y.rs", &rust_functions(5))?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let edited = base.replace("let value = 1;", "let value = 100;");
    repo.write("x.rs", &edited)?;
    repo.commit("edit")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    repo.switch("main")?;
    repo.git(&["mv", "x.rs", "x2.rs"])?;
    repo.commit("rename")?;
    repo.branch_at("feature", "main")?;
    repo.write("x2.rs", &edited)?;
    repo.commit("rebased")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    assert_that!(units.len(), eq(1));
    assert!(is_reviewed(units[0].state), "{units:#?}");
    Ok(())
}

/// An edit repeated in a file whose context does not distinguish it reopens.
#[test]
fn indistinguishable_repeated_edit_reopens() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let block = indoc! {"
        fn same() {
            a();
            b();
            c();
            d();
            e();
        }
    "};
    let base = format!("{}fn tail() {{}}\n", block.repeat(4));
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let edited_block = block.replace("c();", "c(1);");
    let edited = format!("{edited_block}{block}{edited_block}{block}fn tail() {{}}\n");
    repo.write("src/lib.rs", &edited)?;
    repo.commit("repeat an edit")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    repo.write(
        "src/lib.rs",
        &edited.replace("fn tail() {}", "fn tail() { x(); }"),
    )?;
    repo.commit("unrelated")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    let repeated: Vec<_> = units
        .iter()
        .filter(|unit| unit.added.contains("c(1);"))
        .collect();
    assert_that!(repeated.len(), eq(2));
    assert!(
        repeated.iter().all(|unit| !is_reviewed(unit.state)),
        "{repeated:#?}"
    );
    Ok(())
}

/// An explicit `unseen` on an inherited mark keeps the unit unreviewed in later revisions.
#[test]
fn unseen_stops_inheritance() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(6);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let one = base.replace("let value = 1;", "let value = 100;");
    repo.write("src/lib.rs", &one)?;
    repo.commit("one")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    let two = one.replace("let value = 4;", "let value = 400;");
    repo.write("src/lib.rs", &two)?;
    repo.commit("two")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    let inherited = find(&units, "100")?;
    assert!(matches!(
        inherited.state,
        UnitState::Reviewed {
            inherited: true,
            ..
        }
    ));
    review.mark(&history.view(second)?, &[inherited.id], MarkVerdict::Unseen)?;

    repo.write(
        "src/lib.rs",
        &two.replace("let value = 5;", "let value = 500;"),
    )?;
    repo.commit("three")?;
    let third = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, third)?;
    let unit = find(&units, "100")?;
    assert!(
        matches!(unit.state, UnitState::Unseen { .. }),
        "{:?}",
        unit.state
    );
    assert_that!(history.view(third)?.status().total.new, eq(3));
    Ok(())
}

/// Two reviewed chunks that merge into one reopen; the note on one of them falls back to the
/// file and keeps its provenance.
#[test]
fn merged_chunks_reopen_and_notes_keep_provenance() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(4);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let two_chunks = base.replace(
        indoc! {"
                let value = 1;
                value
            }
        "},
        indoc! {"
                let value = 10;
                value
            } // end
        "},
    );
    repo.write("src/lib.rs", &two_chunks)?;
    repo.commit("two chunks")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;
    let history = review.history()?;
    let units = summaries(&history, first)?;
    assert_that!(units.len(), eq(2));
    let noted_unit = find(&units, "let value = 10;")?.id;
    review.note(&history.view(first)?, noted_unit, "why ten?")?;

    repo.write(
        "src/lib.rs",
        &two_chunks.replace(
            indoc! {"
                \x20   let value = 10;
                    value
            "},
            indoc! {"
                \x20   let value = 10;
                    value + 0
            "},
        ),
    )?;
    repo.commit("merge the chunks")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    assert_that!(units.len(), eq(1));
    assert!(!is_reviewed(units[0].state), "{:?}", units[0].state);
    let notes = history.view(second)?.notes();
    assert_that!(notes.len(), eq(1));
    assert_that!(notes[0].written_on, eq(noted_unit));
    assert_that!(notes[0].text.as_str(), eq("why ten?"));
    assert_that!(
        notes[0].attached,
        eq(&NoteAttachment::File {
            path: "src/lib.rs".to_owned()
        })
    );
    Ok(())
}

/// A reviewed chunk that splits in two reopens both parts, and its note falls back to the file.
#[test]
fn split_chunk_reopens() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(4);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let one_chunk = base.replace(
        indoc! {"
                let value = 1;
                value
            }
        "},
        indoc! {"
                let value = 10;
                value + 0
            } // end
        "},
    );
    repo.write("src/lib.rs", &one_chunk)?;
    repo.commit("one chunk")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;
    let history = review.history()?;
    let units = summaries(&history, first)?;
    assert_that!(units.len(), eq(1));
    review.note(&history.view(first)?, units[0].id, "check this")?;

    repo.write(
        "src/lib.rs",
        &one_chunk.replace("    value + 0\n", "    value\n"),
    )?;
    repo.commit("split")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    assert_that!(units.len(), eq(2));
    assert!(
        units.iter().all(|unit| !is_reviewed(unit.state)),
        "{units:#?}"
    );
    let notes = history.view(second)?.notes();
    assert!(notes[0].is_outdated());
    Ok(())
}

/// A note on an unchanged unit follows it to the next revision.
#[test]
fn notes_follow_their_unit() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(6);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let one = base.replace("let value = 1;", "let value = 100;");
    repo.write("src/lib.rs", &one)?;
    repo.commit("one")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    let history = review.history()?;
    let unit = summaries(&history, first)?[0].id;
    review.note(&history.view(first)?, unit, "fine")?;
    repo.write(
        "src/lib.rs",
        &one.replace("let value = 4;", "let value = 400;"),
    )?;
    repo.commit("two")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let units = summaries(&history, second)?;
    let notes = history.view(second)?.notes();
    assert_that!(
        notes[0].attached,
        eq(&NoteAttachment::Unit {
            unit: find(&units, "100")?.id
        })
    );
    Ok(())
}

/// Unit IDs are revision-local: an ID of another revision is rejected.
#[test]
fn unit_ids_of_other_revisions_are_rejected() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(3);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 100;"),
    )?;
    repo.commit("one")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 101;"),
    )?;
    repo.commit("two")?;
    let second = refresh(&review)?;
    let history = review.history()?;
    let old = summaries(&history, first)?[0].id;
    let result = review.mark(&history.view(second)?, &[old], MarkVerdict::Seen);
    assert!(
        matches!(result, Err(review_core::Error::WrongRevision { .. })),
        "{result:?}"
    );
    Ok(())
}

/// Observing A, then B, then a force-push back to A records A again as revision 3: revisions
/// form a chain in observation order, and the current revision is the last one.
#[test]
fn force_push_back_to_earlier_head_is_a_new_revision() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(4);
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 10;"),
    )?;
    let head_a = repo.commit("A")?;
    let review = repo.review()?;
    let first = refresh(&review)?;

    repo.write(
        "src/lib.rs",
        &base.replace("let value = 2;", "let value = 20;"),
    )?;
    repo.commit("B")?;
    let second = refresh(&review)?;
    mark_all(&review, second, MarkVerdict::Seen)?;

    repo.branch_at("feature", &head_a)?;
    let back = review.refresh()?;
    assert_that!(back.number.get(), eq(3));
    assert!(back.created);
    let history = review.history()?;
    assert_that!(history.current(), some(eq(back.number)));
    let records = history.revisions();
    assert_that!(records[2].head, eq(&records[0].head));
    let view = history.current_view()?;
    // A's edit in f1 is new again: B, the previous revision, did not have it.
    assert_that!(view.status().total.unreviewed(), eq(1));
    assert_that!(
        summaries(&history, first)?[0].unit.kind,
        eq(&summaries(&history, back.number)?[0].unit.kind)
    );

    // Observing the current head again records nothing.
    let again = review.refresh()?;
    assert!(!again.created);
    assert_that!(again.number, eq(back.number));
    Ok(())
}

/// The scenario that motivated the linear chain: X accepted on B, then marked `unseen` after
/// returning to A, then C keeps X. The newer `unseen` wins; with a `flagged` verdict instead, the
/// flag wins over the older acceptance.
#[test]
fn newer_verdict_after_return_is_not_overtaken() -> eyre::Result<()> {
    for (verdict, expected_name) in [
        (MarkVerdict::Unseen, "unseen"),
        (MarkVerdict::Flagged, "flagged"),
    ] {
        let repo = TestRepo::new()?;
        let base = rust_functions(5);
        repo.write("src/lib.rs", &base)?;
        repo.commit("base")?;
        repo.branch_at("feature", "main")?;
        let with_x = base.replace("let value = 1;", "let value = 10;");
        repo.write("src/lib.rs", &with_x)?;
        let head_a = repo.commit("A has X")?;
        let review = repo.review()?;
        refresh(&review)?;

        // B keeps X and adds an unrelated change; X is accepted there.
        repo.write(
            "src/lib.rs",
            &with_x.replace("let value = 3;", "let value = 30;"),
        )?;
        repo.commit("B")?;
        let on_b = refresh(&review)?;
        let history = review.history()?;
        let x_on_b = find(&summaries(&history, on_b)?, "let value = 10;")?.id;
        review.mark(&history.view(on_b)?, &[x_on_b], MarkVerdict::Accepted)?;

        // Back to A, which is a new revision; X gets the newer verdict there.
        repo.branch_at("feature", &head_a)?;
        let back = refresh(&review)?;
        let history = review.history()?;
        let x_back = find(&summaries(&history, back)?, "let value = 10;")?.id;
        review.mark(&history.view(back)?, &[x_back], verdict)?;

        // C keeps X with another unrelated change.
        repo.write(
            "src/lib.rs",
            &with_x.replace("let value = 4;", "let value = 40;"),
        )?;
        repo.commit("C")?;
        let on_c = refresh(&review)?;
        let history = review.history()?;
        let units = summaries(&history, on_c)?;
        let x_on_c = find(&units, "let value = 10;")?;
        assert_that!(
            x_on_c.state.name(),
            eq(expected_name),
            "{verdict:?}: {:?}",
            x_on_c.state
        );
    }
    Ok(())
}

/// Verdicts go only on the current revision; marking an older one is rejected and records
/// nothing, while notes may still go on any revision.
#[test]
fn marking_a_non_current_revision_is_rejected() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = rust_functions(3);
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
    let stale = review.history()?;
    repo.write(
        "src/lib.rs",
        &base.replace("let value = 1;", "let value = 11;"),
    )?;
    repo.commit("two")?;
    refresh(&review)?;

    // A view taken before revision 2 existed is rejected too: the check runs under the lock.
    let view = stale.view(first)?;
    let unit = view.unit_ids()[0];
    let result = review.mark(&view, &[unit], MarkVerdict::Seen);
    assert!(
        matches!(result, Err(review_core::Error::NotCurrentRevision { .. })),
        "{result:?}"
    );
    let history = review.history()?;
    let old_view = history.view(first)?;
    assert!(matches!(
        review.mark(&old_view, &[unit], MarkVerdict::Seen),
        Err(review_core::Error::NotCurrentRevision { .. })
    ));
    assert!(!history.view(first)?.state(unit)?.is_reviewed());
    review.note(&old_view, unit, "still allowed")?;
    Ok(())
}

/// A change of only the line ending reopens the unit, and the interdiff keeps the endings.
#[test]
fn line_ending_changes_stay_visible_in_interdiffs() -> eyre::Result<()> {
    let repo = TestRepo::new()?;
    let base = format!("{}const LAST: u8 = 0;\n", rust_functions(3));
    repo.write("src/lib.rs", &base)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    let edited = base.replace("LAST: u8 = 0;", "LAST: u8 = 1;");
    repo.write("src/lib.rs", &edited)?;
    repo.commit("edit")?;
    let review = repo.review()?;
    let first = refresh(&review)?;
    mark_all(&review, first, MarkVerdict::Seen)?;

    let endings = |revision| -> eyre::Result<Vec<(DiffLineKind, LineEnding)>> {
        let history = review.history()?;
        let view = history.view(revision)?;
        let unit = view.unit_ids()[0];
        let interdiff = view
            .interdiff(unit)?
            .ok_or_else(|| eyre::eyre!("no interdiff"))?;
        assert!(
            interdiff
                .head
                .lines
                .iter()
                .all(|line| line.text == "const LAST: u8 = 1;")
        );
        Ok(interdiff
            .head
            .lines
            .iter()
            .map(|line| (line.kind, line.ending))
            .collect())
    };
    repo.write("src/lib.rs", edited.trim_end_matches('\n'))?;
    repo.commit("drop the final newline")?;
    let second = refresh(&review)?;
    assert_that!(
        endings(second)?,
        elements_are![
            eq(&(DiffLineKind::Old, LineEnding::Lf)),
            eq(&(DiffLineKind::New, LineEnding::Missing))
        ]
    );

    repo.write(
        "src/lib.rs",
        &edited.replace("LAST: u8 = 1;\n", "LAST: u8 = 1;\r\n"),
    )?;
    repo.commit("CRLF")?;
    let third = refresh(&review)?;
    assert_that!(
        endings(third)?,
        elements_are![
            eq(&(DiffLineKind::Old, LineEnding::Lf)),
            eq(&(DiffLineKind::New, LineEnding::CrLf))
        ]
    );
    Ok(())
}
