//! Marking whole files and directories, and the directory tree's aggregated counts.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::tree::{NodeKind, file_tree};
use review_core::{Error, MarkVerdict, UnitState, Verdict};

use crate::common::{TestRepo, refresh};

/// Writes the files every revision of these tests starts from, on `main`, and branches
/// `feature` with one change in each.
fn repo_with_tree() -> eyre::Result<TestRepo> {
    let repo = TestRepo::new()?;
    for path in ["top.txt", "dir/b.txt", "dir/sub/c.txt", "other/d.txt"] {
        repo.write(path, "one\ntwo\nthree\n")?;
    }
    let base = repo.commit("base")?;
    repo.branch_at("feature", &base)?;
    for path in ["top.txt", "dir/b.txt", "dir/sub/c.txt", "other/d.txt"] {
        repo.write(path, "one\nTWO\nthree\n")?;
    }
    repo.commit("change all")?;
    Ok(repo)
}

/// Marking a directory marks exactly the units below it, in one call, and the tree sums them.
#[test]
fn marking_a_directory_marks_the_units_below_it() -> eyre::Result<()> {
    let repo = repo_with_tree()?;
    let review = repo.review()?;
    refresh(&review)?;
    let history = review.history()?;
    let view = history.current_view()?;

    let marked = review.mark_files(&view, &["dir".to_owned()], MarkVerdict::Accepted)?;
    assert_that!(marked.len(), eq(2));

    let history = review.history()?;
    let view = history.current_view()?;
    let accepted = |path: &str| {
        view.units_under(&[path.to_owned()])
            .into_iter()
            .all(|unit| {
                matches!(
                    view.state(unit),
                    Ok(UnitState::Reviewed {
                        verdict: Verdict::Accepted,
                        ..
                    })
                )
            })
    };
    // Everything below dir/ is accepted, nothing else is.
    assert!(accepted("dir/b.txt") && accepted("dir/sub/c.txt"));
    assert!(!accepted("top.txt") && !accepted("other/d.txt"));

    // The tree sums counts per directory, directories first.
    let nodes = file_tree(&view.status().files);
    let dir = nodes.iter().find(|node| node.path == "dir").unwrap();
    assert_that!(dir.kind, eq(NodeKind::Directory));
    assert_that!((dir.counts.accepted, dir.counts.units), eq((2, 2)));
    assert_that!(
        nodes.first().map(|node| node.path.as_str()),
        some(eq("dir"))
    );
    Ok(())
}

/// A file mark covers only the units of the revision it was made on: a unit that appears in a
/// later push to the same directory stays new, and stale views are refused.
#[test]
fn file_marks_never_cover_later_units() -> eyre::Result<()> {
    let repo = repo_with_tree()?;
    let review = repo.review()?;
    refresh(&review)?;
    let first = review.history()?;
    review.mark_files(
        &first.current_view()?,
        &["dir".to_owned()],
        MarkVerdict::Seen,
    )?;

    repo.write("dir/new.txt", "fresh\n")?;
    repo.commit("add a file")?;
    refresh(&review)?;
    let history = review.history()?;
    let view = history.current_view()?;
    let new_units = view.units_under(&["dir/new.txt".to_owned()]);
    assert_that!(new_units.len(), eq(1));
    assert_that!(view.state(new_units[0])?, eq(UnitState::New));

    // Marking through the older view is rejected.
    let stale = review.mark_files(
        &first.current_view()?,
        &["other".to_owned()],
        MarkVerdict::Seen,
    );
    assert!(
        matches!(stale, Err(Error::NotCurrentRevision { .. })),
        "{stale:?}"
    );

    // A path without changes is an error, not a silent no-op.
    let none = review.mark_files(&view, &["nowhere".to_owned()], MarkVerdict::Seen);
    assert!(matches!(none, Err(Error::NoChangedFiles(_))), "{none:?}");
    Ok(())
}
