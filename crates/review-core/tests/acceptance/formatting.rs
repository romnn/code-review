//! Format-independent classification, end to end through git.

use color_eyre::eyre;
use googletest::prelude::*;

use crate::common::{TestRepo, latest_analysis, refresh};

/// Commits `before` on `main` and `after` on `feature` at `path`, and returns the analysis.
fn analyze_change(
    path: &str,
    before: &str,
    after: &str,
) -> eyre::Result<(TestRepo, review_core::analysis::Analysis)> {
    let repo = TestRepo::new()?;
    repo.write(path, before)?;
    repo.commit("base")?;
    repo.branch_at("feature", "main")?;
    repo.write(path, after)?;
    repo.commit("change")?;
    let review = repo.review()?;
    refresh(&review)?;
    let analysis = latest_analysis(&review)?;
    Ok((repo, analysis))
}

fn formatting_flags(analysis: &review_core::analysis::Analysis) -> Vec<bool> {
    analysis
        .units
        .iter()
        .map(|unit| unit.formatting_only)
        .collect()
}

/// Re-wrapping a Rust call is formatting-only, although `git diff -w` shows it.
#[test]
fn rewrapped_rust_call_is_formatting_only() -> eyre::Result<()> {
    let (repo, analysis) = analyze_change(
        "src/lib.rs",
        "fn main() {\n    let total = compute(alpha, beta, gamma);\n}\n",
        "fn main() {\n    let total = compute(\n        alpha,\n        beta,\n        gamma\n    );\n}\n",
    )?;
    assert_that!(formatting_flags(&analysis), elements_are![eq(&true)]);
    assert!(analysis.files[0].formatting_only);
    let whitespace_blind = repo.git(&["diff", "-w", "main", "feature"])?;
    assert!(!whitespace_blind.is_empty(), "git diff -w hides the change");
    Ok(())
}

/// A re-wrapped call next to one real edit: the file is not formatting-only, the re-wrap alone
/// is proven, and only the edited token is highlighted.
#[test]
fn real_edit_keeps_the_rewrap_proven_and_is_highlighted() -> eyre::Result<()> {
    let before = "fn main() {\n    let total = compute(alpha, beta, gamma);\n    let other = 0;\n    let limit = 1;\n    run(total, limit);\n}\n";
    let after = "fn main() {\n    let total = compute(\n        alpha,\n        beta,\n        gamma\n    );\n    let other = 0;\n    let limit = 2;\n    run(total, limit);\n}\n";
    let (_repo, analysis) = analyze_change("src/lib.rs", before, after)?;
    assert!(!analysis.files[0].formatting_only);
    assert_that!(
        formatting_flags(&analysis),
        elements_are![eq(&true), eq(&false)]
    );
    let edit = analysis
        .units
        .iter()
        .filter_map(|unit| unit.chunk())
        .find(|chunk| chunk.added.contains("limit = 2"))
        .ok_or_else(|| eyre::eyre!("no edit chunk"))?;
    let highlighted: Vec<&str> = edit
        .added_highlights
        .iter()
        .map(|range| &edit.added[range.clone()])
        .collect();
    let removed: Vec<&str> = edit
        .removed_highlights
        .iter()
        .map(|range| &edit.removed[range.clone()])
        .collect();
    // Only the changed literal is highlighted.
    assert_that!(highlighted, elements_are![eq(&"2")]);
    assert_that!(removed, elements_are![eq(&"1")]);
    // The re-wrapped call stays visible but has no highlighted tokens.
    let rewrap = analysis
        .units
        .iter()
        .filter_map(|unit| unit.chunk())
        .find(|chunk| chunk.added.contains("compute("))
        .ok_or_else(|| eyre::eyre!("no re-wrap chunk"))?;
    assert!(rewrap.added_highlights.is_empty() && rewrap.whitespace_only);
    Ok(())
}

/// JavaScript `return isAdmin;` to `return\nisAdmin;` changes meaning through automatic
/// semicolon insertion.
#[test]
fn javascript_return_line_break_is_not_formatting_only() -> eyre::Result<()> {
    let (_repo, analysis) = analyze_change(
        "auth.js",
        "function check() {\n  return isAdmin;\n}\n",
        "function check() {\n  return\n  isAdmin;\n}\n",
    )?;
    assert!(analysis.units.iter().all(|unit| !unit.formatting_only));
    Ok(())
}

/// Re-indenting a Python block into the preceding `if` changes the program, and so does
/// dedenting one statement out of an `if` block.
#[test]
fn python_reindent_is_not_formatting_only() -> eyre::Result<()> {
    let (_repo, nested) = analyze_change(
        "main.py",
        "if ready:\n    start()\nfor item in items:\n    handle(item)\n",
        "if ready:\n    start()\n    for item in items:\n        handle(item)\n",
    )?;
    assert!(nested.units.iter().all(|unit| !unit.formatting_only));
    let (_repo, dedented) = analyze_change(
        "main.py",
        "if ready:\n    start()\n    finish()\n",
        "if ready:\n    start()\nfinish()\n",
    )?;
    assert!(dedented.units.iter().all(|unit| !unit.formatting_only));
    Ok(())
}

/// A whitespace change inside a multi-line string literal is not formatting-only.
#[test]
fn multiline_string_change_is_not_formatting_only() -> eyre::Result<()> {
    let (_repo, analysis) = analyze_change(
        "src/lib.rs",
        "const HELP: &str = \"usage:\n  run\n\";\n",
        "const HELP: &str = \"usage:\n    run\n\";\n",
    )?;
    assert!(analysis.units.iter().all(|unit| !unit.formatting_only));
    Ok(())
}

/// An unsupported language is never formatting-only, even for a pure whitespace change; the
/// change is only hinted as whitespace-only.
#[test]
fn unsupported_language_is_never_formatting_only() -> eyre::Result<()> {
    let (_repo, analysis) = analyze_change("config.yaml", "a:\n  b: 1\n", "a:\n    b: 1\n")?;
    assert!(analysis.units.iter().all(|unit| !unit.formatting_only));
    assert!(
        analysis
            .units
            .iter()
            .filter_map(|unit| unit.chunk())
            .all(|chunk| chunk.whitespace_only)
    );
    Ok(())
}
