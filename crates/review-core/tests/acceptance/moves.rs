//! Move detection.

use color_eyre::eyre;
use googletest::prelude::*;
use indoc::{formatdoc, indoc};
use review_core::analysis::MoveKind;
use review_core::{MoveId, UnitState};

use crate::common::{TestRepo, refresh, summaries};

const FUNCTION: &str = indoc! {"
    fn moved_function(input: u32) -> u32 {
        let doubled = input * 2;
        let shifted = doubled + 7;
        let masked = shifted & 0xff;
        let scaled = masked * 3;
        scaled - 1
    }
"};

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

/// `text` with each line indented by `by`.
fn indent(text: &str, by: &str) -> String {
    let mut indented = String::with_capacity(text.len());
    for line in text.lines() {
        indented.push_str(by);
        indented.push_str(line);
        indented.push('\n');
    }
    indented
}

/// A function moved to another file is one move, and both its units are covered.
#[test]
fn function_moved_to_another_file_is_one_move() -> eyre::Result<()> {
    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &formatdoc!(
                    "
                        fn keep() {{}}
                        {FUNCTION}fn tail() {{}}
                    "
                ),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            (
                "a.rs",
                indoc! {"
                    fn keep() {}
                    fn tail() {}
                "},
            ),
            (
                "b.rs",
                &formatdoc!(
                    "
                        fn other() {{}}
                        {FUNCTION}"
                ),
            ),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].kind, eq(MoveKind::Moved));
    assert_that!(moves[0].source.location.path.as_str(), eq("a.rs"));
    assert_that!(moves[0].target.location.path.as_str(), eq("b.rs"));
    let analysis = view.analysis();
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
                &formatdoc!(
                    "
                        fn keep() {{}}
                        {FUNCTION}fn tail() {{}}
                    "
                ),
            ),
            ("b.rs", "fn b() {}\n"),
            ("c.rs", "fn c() {}\n"),
        ],
        &[
            (
                "a.rs",
                indoc! {"
                    fn keep() {}
                    fn tail() {}
                "},
            ),
            (
                "b.rs",
                &formatdoc!(
                    "
                        fn b() {{}}
                        {FUNCTION}"
                ),
            ),
            (
                "c.rs",
                &formatdoc!(
                    "
                        fn c() {{}}
                        {FUNCTION}"
                ),
            ),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].kind, eq(MoveKind::Moved));
    let analysis = view.analysis();
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
                &formatdoc!(
                    "
                        fn keep() {{}}
                        {FUNCTION}fn tail() {{}}
                    "
                ),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            (
                "a.rs",
                indoc! {"
                    fn keep() {}
                    fn tail() {}
                "},
            ),
            (
                "b.rs",
                &formatdoc!(
                    "
                        fn other() {{}}
                        {edited}"
                ),
            ),
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
    let analysis = view.analysis();
    assert!(analysis.units.iter().all(|unit| !unit.moved));
    assert!(matches!(
        review.acknowledge_move(&view, moves[0].id),
        Err(review_core::Error::MoveHasEdits(_))
    ));
    Ok(())
}

/// A YAML block moved past another key and re-nested under it is a move with edits, never a
/// plain one.
#[test]
fn renested_yaml_block_is_not_a_plain_move() -> eyre::Result<()> {
    let block = indoc! {"
        server:
          host: example.com
          port: 8080
          timeout: 30
          retries: 5
          backoff: exponential
          protocol: https
    "};
    let nested = block
        .lines()
        .map(|line| format!("  {line}\n"))
        .collect::<Vec<_>>()
        .concat();
    let (_repo, review) = setup(
        &[(
            "config.yaml",
            &formatdoc!(
                "
                    {block}top:
                      a: 1
                "
            ),
        )],
        &[(
            "config.yaml",
            &formatdoc!(
                "
                    top:
                      a: 1
                    {nested}"
            ),
        )],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let kinds: Vec<MoveKind> = view.moves().iter().map(|found| found.kind).collect();
    assert_that!(kinds, elements_are![eq(&MoveKind::MovedWithEdits)]);
    let analysis = view.analysis();
    assert!(
        analysis
            .units
            .iter()
            .all(|unit| !unit.moved && !unit.formatting_only)
    );
    Ok(())
}

/// A Rust function moved into a module, re-indented with it and with one of its lines wrapped
/// anew as a formatter does at the deeper indent, differs only in formatting: a plain move.
#[test]
fn reformatted_rust_move_is_a_plain_move() -> eyre::Result<()> {
    let wrapped = indoc! {"
        mod inner {
            fn moved_function(input: u32) -> u32 {
                let doubled = input * 2;
                let shifted = doubled + 7;
                let masked =
                    shifted & 0xff;
                let scaled = masked * 3;
                scaled - 1
            }
        }
    "};
    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &formatdoc!(
                    "
                        fn keep() {{}}
                        {FUNCTION}fn tail() {{}}
                    "
                ),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            (
                "a.rs",
                indoc! {"
                    fn keep() {}
                    fn tail() {}
                "},
            ),
            (
                "b.rs",
                &formatdoc!(
                    "
                        fn other() {{}}
                        {wrapped}"
                ),
            ),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].kind, eq(MoveKind::Moved));
    assert!(moves[0].inner.is_none());
    Ok(())
}

/// A Python function moved into a class is re-indented too, but there it becomes a method, so
/// its syntax tree changes: it stays a move with edits.
#[test]
fn python_moved_into_a_class_keeps_its_edits() -> eyre::Result<()> {
    let function = indoc! {"
        def moved_function(value):
            doubled = value * 2
            shifted = doubled + 7
            masked = shifted & 0xFF
            scaled = masked * 3
            return scaled - 1
    "};
    let method = function
        .lines()
        .map(|line| format!("    {line}\n"))
        .collect::<Vec<_>>()
        .concat();
    let (_repo, review) = setup(
        &[(
            "a.py",
            &formatdoc!(
                "
                    {function}

                    class Holder:
                        pass
                "
            ),
        )],
        &[(
            "a.py",
            &formatdoc!(
                "
                    class Holder:
                        pass

                    {method}"
            ),
        )],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let kinds: Vec<MoveKind> = view.moves().iter().map(|found| found.kind).collect();
    assert_that!(kinds, elements_are![eq(&MoveKind::MovedWithEdits)]);
    Ok(())
}

/// Code a new call wraps where it stands is re-indented in place, not moved: its chunk keeps it
/// where it was, so no move claims it.
#[test]
fn code_reindented_in_place_is_no_move() -> eyre::Result<()> {
    let (_repo, review) = setup(
        &[(
            "a.rs",
            indoc! {"
                fn reasoning(id: &str, summary: &[&str]) -> Item {
                    Item::Reasoning(Box::new(ReasoningItem {
                        content: Some(Vec::new()),
                        id: id.to_owned(),
                        summary: Some(summary.iter().map(|&line| line.to_owned()).collect()),
                    }))
                }
            "},
        )],
        &[(
            "a.rs",
            indoc! {"
                fn reasoning(id: &str, summary: &[&str]) -> Item {
                    item(ItemVariant::Reasoning(Box::new(
                        ReasoningItem {
                            content: Some(Vec::new()),
                            id: id.to_owned(),
                            summary: Some(summary.iter().map(|&line| line.to_owned()).collect()),
                        },
                    )))
                }
            "},
        )],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    assert_that!(view.moves(), is_empty());
    Ok(())
}

/// A function that leaves a block as the block's closing line moves above it crosses only that
/// line and a blank one, which stay: its removed and added lines fall in two chunks, but it went
/// nowhere, so it is no move.
#[test]
fn code_crossing_only_a_closing_line_is_no_move() -> eyre::Result<()> {
    let (_repo, review) = setup(
        &[(
            "a.js",
            indoc! {r#"
                describe("ledger", () => {
                  it("applies", () => {});
                  function helper(input) {
                    const doubled = input * 2;
                    const shifted = doubled + 7;
                    return doubled + shifted;
                  }
                });

                run();
            "#},
        )],
        &[(
            "a.js",
            indoc! {r#"
                describe("ledger", () => {
                  it("applies", () => {});
                });

                function helper(input) {
                  const doubled = input * 2;
                  const shifted = doubled + 7;
                  return doubled + shifted;
                }

                run();
            "#},
        )],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let analysis = view.analysis();
    assert_that!(
        analysis
            .units
            .iter()
            .filter(|unit| unit.chunk().is_some())
            .count(),
        eq(2)
    );
    assert_that!(view.moves(), is_empty());
    Ok(())
}

/// Code wrapped in place may also be copied elsewhere, verbatim as it was, into an existing
/// function: the copy is longer than any run left in place, but the code stayed, so the copy is
/// an addition, not a move that could fold it.
#[test]
fn a_copy_of_code_that_stayed_is_no_move() -> eyre::Result<()> {
    let expression = indoc! {"
        Item::Reasoning(Box::new(ReasoningItem {
            content: Some(Vec::new()),
            id: id.to_owned(),
            summary: Some(summary.iter().map(|&line| line.to_owned()).collect()),
        }))
    "};
    let in_function = |body: &str| {
        formatdoc!(
            "
                fn reasoning(id: &str, summary: &[&str]) -> Item {{
                {body}}}
            ",
            body = indent(body, "    "),
        )
    };
    let (_repo, review) = setup(
        &[
            ("a.rs", &in_function(expression)),
            ("b.rs", &in_function("log(id);\n")),
        ],
        &[
            (
                "a.rs",
                &in_function(indoc! {"
                    item(ItemVariant::Reasoning(Box::new(
                        ReasoningItem {
                            content: Some(Vec::new()),
                            id: id.to_owned(),
                            summary: Some(summary.iter().map(|&line| line.to_owned()).collect()),
                        },
                    )))
                "}),
            ),
            ("b.rs", &in_function(&["log(id);\n", expression].concat())),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    assert_that!(view.moves(), is_empty());
    Ok(())
}

/// Extracts the code around an `audit` call that stays where it was into an existing function of
/// b.rs, with `between` written between the two extracted runs there.
fn extract_around_a_call(between: &str) -> eyre::Result<(TestRepo, review_core::Review)> {
    let before = indoc! {"
        let total = entries.len();
        let valid = entries.iter().filter(|entry| entry.valid).count();
        let ratio = valid as f64 / total.max(1) as f64;
    "};
    let after = indoc! {r#"
        let skipped = total - valid;
        let message = format!("{valid} of {total} valid, {skipped} skipped");
        println!("{message} at {ratio}");
    "#};
    let audit = "audit(\"process\", entries.len());\n";
    let function = |name: &str, body: &str| {
        formatdoc!(
            "
                fn {name}(entries: &[Entry]) {{
                {body}}}
            ",
            body = indent(body, "    "),
        )
    };
    setup(
        &[
            (
                "a.rs",
                &function("process", &[before, audit, after].concat()),
            ),
            ("b.rs", &function("summary", "log(entries);\n")),
        ],
        &[
            (
                "a.rs",
                &function(
                    "process",
                    &formatdoc!(
                        "
                            if !entries.is_empty() {{
                            {audit}}}
                        ",
                        audit = indent(audit, "    "),
                    ),
                ),
            ),
            (
                "b.rs",
                &function(
                    "summary",
                    &["log(entries);\n", before, between, after].concat(),
                ),
            ),
        ],
    )
}

/// Code extracted from around a call that stays where it was moves as two runs, never merged
/// across that call, so a copy of the call between them at the destination stays an addition.
#[test]
fn a_move_never_merges_across_code_that_stayed() -> eyre::Result<()> {
    let (_repo, review) = extract_around_a_call("audit(\"process\", entries.len());\n")?;
    let history = review.history()?;
    let view = history.current_view()?;
    let targets: Vec<(usize, usize)> = view
        .moves()
        .iter()
        .filter(|found| found.target.location.path == "b.rs")
        .map(|found| (found.target.location.line, found.target.location.end_line))
        .collect();
    // b.rs holds the first run on lines 3–5, the call on line 6 and the second run on 7–9.
    assert_that!(targets, unordered_elements_are![eq(&(3, 5)), eq(&(7, 9))]);
    Ok(())
}

/// Two plain moves that land side by side make up one added chunk between them, but neither
/// covers it, and acknowledging either marks nothing there: the chunk is not folded as moved.
#[test]
fn a_chunk_two_moves_cover_together_is_not_folded() -> eyre::Result<()> {
    let (_repo, review) = extract_around_a_call("")?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(2));
    assert!(moves.iter().all(|found| found.kind == MoveKind::Moved));
    let analysis = view.analysis();
    let destination = view
        .units_of_file("b.rs")
        .into_iter()
        .find(|unit| {
            analysis
                .unit(unit.index())
                .is_some_and(|unit| unit.chunk().is_some())
        })
        .ok_or_else(|| eyre::eyre!("no chunk in b.rs"))?;
    // Together the moves cover the whole chunk, lines 3–8 of b.rs.
    let targets: Vec<(usize, usize)> = moves
        .iter()
        .map(|found| (found.target.location.line, found.target.location.end_line))
        .collect();
    assert_that!(targets, unordered_elements_are![eq(&(3, 5)), eq(&(6, 8))]);
    let chunk = analysis
        .unit(destination.index())
        .and_then(|unit| unit.chunk())
        .ok_or_else(|| eyre::eyre!("no chunk"))?;
    assert_that!((chunk.new.start, chunk.new.len), eq((2, 6)));
    assert!(
        analysis
            .unit(destination.index())
            .is_some_and(|unit| !unit.moved)
    );
    assert_that!(view.collapse(destination)?, none());
    for found in &moves {
        assert!(!view.units_covered_by_move(found.id)?.contains(&destination));
    }
    Ok(())
}

/// A chunk the syntax proof shows only reformatted kept all of its code, rewrapped lines too, so
/// none of it pairs with a copy of the old code elsewhere.
#[test]
fn a_copy_of_reformatted_code_is_no_move() -> eyre::Result<()> {
    let statements = indoc! {"
        let total = input.iter().copied().map(u64::from).sum::<u64>();
        let largest = input.iter().copied().max().unwrap_or_default();
        let smallest = input.iter().copied().min().unwrap_or_default();
    "};
    let rest = indoc! {"
        let spread = u64::from(largest - smallest);
        let doubled = spread * 2;
        let halved = total / 2;
        let sum = doubled + halved;
        sum.max(1)
    "};
    let wrapped = indoc! {"
        let total = input
            .iter()
            .copied()
            .map(u64::from)
            .sum::<u64>();
        let largest = input
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        let smallest = input
            .iter()
            .copied()
            .min()
            .unwrap_or_default();
    "};
    let function = |name: &str, body: &str| {
        formatdoc!(
            "
                fn {name}(input: &[u32]) -> u64 {{
                {body}}}
            ",
            body = indent(body, "    "),
        )
    };
    let first_of_rest = indent(rest.lines().next().unwrap_or_default(), "  ");
    let (_repo, review) = setup(
        &[
            (
                "a.rs",
                &function("compute", &[statements, &indent(rest, "  ")].concat()),
            ),
            ("b.rs", &function("other", "log(input);\n")),
        ],
        &[
            ("a.rs", &function("compute", &[wrapped, rest].concat())),
            (
                "b.rs",
                &function(
                    "other",
                    &["log(input);\n", statements, &first_of_rest].concat(),
                ),
            ),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let analysis = view.analysis();
    assert!(
        view.units_of_file("a.rs").iter().all(|unit| analysis
            .unit(unit.index())
            .is_some_and(|unit| unit.formatting_only)),
        "a.rs is only reformatted"
    );
    assert_that!(view.moves(), is_empty());
    Ok(())
}

/// A blank line inside moved code never splits the move, even where a blank line of different
/// whitespace replaces the code where it left.
#[test]
fn a_blank_line_in_moved_code_never_splits_the_move() -> eyre::Result<()> {
    let commands = formatdoc! {"
        cp build/output.tar.gz /srv/releases/current.tar.gz
        tar -xzf /srv/releases/current.tar.gz -C /srv/app
        {spaces}
        systemctl restart app.service --no-block
        curl --fail http://localhost:8080/health
    ", spaces = "  "};
    let (_repo, review) = setup(
        &[
            (
                "a.sh",
                &formatdoc!(
                    "
                        echo start
                        {commands}echo done
                    "
                ),
            ),
            ("b.sh", "echo other\n"),
        ],
        &[
            (
                "a.sh",
                indoc! {"
                    echo start
                    make clean all
                    make install

                    make test
                    make package
                    echo done
                "},
            ),
            (
                "b.sh",
                &formatdoc!(
                    "
                        echo other
                        {commands}"
                ),
            ),
        ],
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let moves = view.moves();
    assert_that!(moves.len(), eq(1));
    assert_that!(moves[0].kind, eq(MoveKind::Moved));
    assert_that!(
        (
            moves[0].target.location.line,
            moves[0].target.location.end_line
        ),
        eq((2, 6))
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
                &formatdoc!(
                    "
                        fn keep() {{}}
                        {FUNCTION}fn tail() {{}}
                    "
                ),
            ),
            ("b.rs", "fn other() {}\n"),
        ],
        &[
            (
                "a.rs",
                indoc! {"
                    fn keep() {}
                    fn tail_renamed() {}
                "},
            ),
            (
                "b.rs",
                &formatdoc!(
                    "
                        fn other() {{}}
                        {FUNCTION}"
                ),
            ),
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
        &[(
            "b.rs",
            &formatdoc!(
                "
                    fn other() {{}}
                    {FUNCTION}"
            ),
        )],
    )?;
    repo.git(&["mv", "old.rs", "new.rs"])?;
    repo.write("new.rs", &filler)?;
    repo.commit("rename and move out")?;
    let review = repo.review()?;
    refresh(&review)?;
    let history = review.history()?;
    let view = history.current_view()?;
    let analysis = view.analysis();
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
                &formatdoc!(
                    "
                        fn keep() {{}}
                        {FUNCTION}fn tail() {{}}
                    "
                ),
            ),
            ("b.rs", "fn other() {}\n"),
            ("Cargo.lock", "version = 3\n"),
            (
                "c.rs",
                indoc! {"
                    fn c1() {}
                    fn c2() {}
                    fn c3() {}
                    fn c4() {}
                "},
            ),
        ],
        &[
            (
                "a.rs",
                indoc! {"
                    fn keep() {}
                    fn tail() {}
                "},
            ),
            (
                "b.rs",
                &formatdoc!(
                    "
                        fn other() {{}}
                        {FUNCTION}"
                ),
            ),
            ("Cargo.lock", "version = 4\n"),
            (
                "c.rs",
                indoc! {"
                    fn c1() { 1 }
                    fn c2() {}
                    fn c3() { 3 }
                    fn c4() {}
                "},
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
