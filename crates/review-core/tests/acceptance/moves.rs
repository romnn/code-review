//! Move detection.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::analysis::MoveKind;
use review_core::{MoveId, UnitState};

use crate::common::{TestRepo, refresh, summaries};

const FUNCTION: &str = "fn moved_function(input: u32) -> u32 {\n    let doubled = input * 2;\n    let shifted = doubled + 7;\n    let masked = shifted & 0xff;\n    let scaled = masked * 3;\n    scaled - 1\n}\n";

/// Commits `files` on `main` and `changed` on `feature` (an empty text deletes the file).
fn setup(
    files: &[(&str, &str)],
    changed: &[(&str, &str)],
) -> eyre::Result<(TestRepo, review_core::Review)> {
    let repo = TestRepo::new()?;
    for (path, text) in files {
        repo.write(path, text)?;
    }
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    for (path, text) in changed {
        repo.write(path, text)?;
    }
    repo.commit("change")?;
    let review = repo.review()?;
    refresh(&review)?;
    Ok((repo, review))
}

/// A function moved to another file is one move, and both its units are covered.
#[test]
fn function_moved_to_another_file_is_one_move() -> eyre::Result<()> {
    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &format!("fn keep() {{}}\n{FUNCTION}fn tail() {{}}\n"),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            ("a.rs", "fn keep() {}\nfn tail() {}\n"),
            ("b.rs", &format!("fn other() {{}}\n{FUNCTION}")),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].kind, eq(MoveKind::Moved));
    assert_that!(moves[0].source.location.path.as_str(), eq("a.rs"));
    assert_that!(moves[0].target.location.path.as_str(), eq("b.rs"));
    let analysis = view.analysis().ok_or_else(|| eyre::eyre!("no analysis"))?;
    assert!(analysis.units.iter().all(|unit| unit.moved));

    // Acknowledging the move marks both units, and nothing else.
    let marked = review.acknowledge_move(&view, moves[0].id)?;
    assert_that!(marked.len(), eq(2));
    let history = review.history()?;
    let view = history.current_view()?;
    assert!(
        view.unit_ids()
            .iter()
            .all(|&id| view.state(id).is_ok_and(UnitState::is_reviewed))
    );
    Ok(())
}

/// A block copied to two places is one move and one addition.
#[test]
fn copied_block_is_one_move_and_one_addition() -> eyre::Result<()> {
    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &format!("fn keep() {{}}\n{FUNCTION}fn tail() {{}}\n"),
            ),
            ("b.rs", "fn b() {}\n"),
            ("c.rs", "fn c() {}\n"),
        ],
        &[
            ("a.rs", "fn keep() {}\nfn tail() {}\n"),
            ("b.rs", &format!("fn b() {{}}\n{FUNCTION}")),
            ("c.rs", &format!("fn c() {{}}\n{FUNCTION}")),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].kind, eq(MoveKind::Moved));
    let analysis = view.analysis().ok_or_else(|| eyre::eyre!("no analysis"))?;
    let covered = analysis.units.iter().filter(|unit| unit.moved).count();
    assert_that!(covered, eq(2));
    assert_that!(analysis.units.len(), eq(3));
    Ok(())
}

/// A moved block with a one-line edit is a move with edits: expanded, not collapsed.
#[test]
fn moved_block_with_edit_is_expanded() -> eyre::Result<()> {
    let edited = FUNCTION.replace(
        "let masked = shifted & 0xff;",
        "let masked = shifted & 0x0f;",
    );
    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &format!("fn keep() {{}}\n{FUNCTION}fn tail() {{}}\n"),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            ("a.rs", "fn keep() {}\nfn tail() {}\n"),
            ("b.rs", &format!("fn other() {{}}\n{edited}")),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].kind, eq(MoveKind::MovedWithEdits));
    assert_that!(
        moves[0].source.location.end_line - moves[0].source.location.line,
        eq(6)
    );
    let analysis = view.analysis().ok_or_else(|| eyre::eyre!("no analysis"))?;
    assert!(analysis.units.iter().all(|unit| !unit.moved));
    assert!(matches!(
        review.acknowledge_move(&view, moves[0].id),
        Err(review_core::Error::MoveHasEdits(_))
    ));
    Ok(())
}

/// A YAML block moved and re-nested is not collapsed as a plain move.
#[test]
fn renested_yaml_block_is_not_a_plain_move() -> eyre::Result<()> {
    let block = "server:\n  host: example.com\n  port: 8080\n  timeout: 30\n  retries: 5\n  backoff: exponential\n";
    let nested = block
        .lines()
        .map(|line| format!("  {line}\n"))
        .collect::<Vec<_>>()
        .concat();
    let (_repo, review) = setup(
        &[("config.yaml", &format!("top:\n  a: 1\n{block}other: x\n"))],
        &[("config.yaml", &format!("top:\n  a: 1\n{nested}other: x\n"))],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    assert!(
        view.moves()
            .iter()
            .all(|found| found.kind != MoveKind::Moved)
    );
    let analysis = view.analysis().ok_or_else(|| eyre::eyre!("no analysis"))?;
    assert!(
        analysis
            .units
            .iter()
            .all(|unit| !unit.moved && !unit.formatting_only)
    );
    Ok(())
}

/// Acknowledging a move that only partly covers a chunk leaves that chunk unreviewed.
#[test]
fn partial_move_acknowledgement_leaves_chunk_unreviewed() -> eyre::Result<()> {
    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &format!("fn keep() {{}}\n{FUNCTION}fn tail() {{}}\n"),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            ("a.rs", "fn keep() {}\nfn tail_renamed() {}\n"),
            ("b.rs", &format!("fn other() {{}}\n{FUNCTION}")),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    let marked = review.acknowledge_move(&view, moves[0].id)?;
    assert_that!(marked, elements_are![eq(&moves[0].target.unit)]);

    let history = review.history()?;
    let units = summaries(&history, view.number())?;
    let source = units
        .iter()
        .find(|unit| unit.path == "a.rs")
        .ok_or_else(|| eyre::eyre!("no source unit"))?;
    assert_that!(source.state, eq(UnitState::New));
    let unknown: MoveId = format!("m{}-0009", view.number()).parse()?;
    assert!(matches!(
        review.acknowledge_move(&history.current_view()?, unknown),
        Err(review_core::Error::UnknownMove(_))
    ));
    Ok(())
}

/// After a rename, a move's source names the old path its base-side line numbers refer to.
#[test]
fn move_source_uses_old_path_after_rename() -> eyre::Result<()> {
    let filler = crate::common::rust_functions(12);
    let (repo, _) = setup(
        &[
            ("old.rs", &format!("{filler}{FUNCTION}")),
            ("b.rs", "fn other() {}\n"),
        ],
        &[("b.rs", &format!("fn other() {{}}\n{FUNCTION}"))],
    )?;
    repo.git(&["mv", "old.rs", "new.rs"])?;
    repo.write("new.rs", &filler)?;
    repo.commit("rename and move out")?;
    let review = repo.review()?;
    refresh(&review)?;
    let history = review.history()?;
    let view = history.current_view()?;
    let analysis = view.analysis().ok_or_else(|| eyre::eyre!("no analysis"))?;
    assert!(
        analysis
            .files
            .iter()
            .any(|file| matches!(file.status, review_core::git::ChangeStatus::Renamed { .. }))
    );
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].source.location.path.as_str(), eq("old.rs"));
    assert_that!(moves[0].target.location.path.as_str(), eq("b.rs"));
    Ok(())
}

/// The engine says how each unit is collapsed, so front ends do not decide it themselves: plain
/// moves name their move, noise names its category, and nearby units share a display hunk.
#[test]
fn collapse_reasons_and_display_hunks() -> eyre::Result<()> {
    use review_core::Collapse;

    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &format!("fn keep() {{}}\n{FUNCTION}fn tail() {{}}\n"),
            ),
            ("b.rs", "fn other() {}\n"),
            ("Cargo.lock", "version = 3\n"),
            ("c.rs", "fn c1() {}\nfn c2() {}\nfn c3() {}\nfn c4() {}\n"),
        ],
        &[
            ("a.rs", "fn keep() {}\nfn tail() {}\n"),
            ("b.rs", &format!("fn other() {{}}\n{FUNCTION}")),
            ("Cargo.lock", "version = 4\n"),
            (
                "c.rs",
                "fn c1() { 1 }\nfn c2() {}\nfn c3() { 3 }\nfn c4() {}\n",
            ),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let by_path = |path: &str| -> Vec<review_core::UnitId> {
        view.unit_ids()
            .into_iter()
            .filter(|&id| view.file(id).is_ok_and(|file| file.display_path() == path))
            .collect()
    };
    let moved = by_path("b.rs")[0];
    match view.collapse(moved)? {
        Some(Collapse::Moved(found)) => assert_that!(found.target.unit, eq(moved)),
        other => return Err(eyre::eyre!("expected a moved collapse, got {other:?}")),
    }
    assert!(matches!(
        view.collapse(by_path("Cargo.lock")[0])?,
        Some(Collapse::Noise(_))
    ));
    let edits = by_path("c.rs");
    assert_that!(edits.len(), eq(2));
    assert_that!(view.collapse(edits[0])?, none());
    assert_that!(view.display_hunks(&edits), elements_are![eq(&edits)]);
    Ok(())
}
