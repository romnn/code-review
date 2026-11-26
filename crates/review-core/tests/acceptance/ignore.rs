//! Which working-tree paths git ignores, as a capture sees them.

use std::collections::HashSet;

use crate::common::TestRepo;
use color_eyre::eyre;
use indoc::indoc;
use review_core::{Repo, Review};

/// Ignored paths are those a capture leaves out: matched by a rule and untracked.
/// A tracked file under an ignored directory is not ignored, and neither is any directory that
/// holds one, so a walk that skips ignored directories reaches it.
#[test]
fn ignored_paths_are_untracked_matches() -> eyre::Result<()> {
    let test = TestRepo::new()?;
    test.write(
        ".gitignore",
        indoc! {"
            target/
            *.log
        "},
    )?;
    test.write("src/lib.rs", "fn main() {}\n")?;
    test.write("target/kept.txt", "kept\n")?;
    std::fs::create_dir_all(test.path().join("target/debug"))?;
    test.git(&["add", ".gitignore", "src/lib.rs"])?;
    test.git(&["add", "--force", "target/kept.txt"])?;
    let review = Review::open_checkout(test.data_dir(), Repo::discover(test.path())?)?;
    let copy = review.ignore_rules().read()?;

    let ignored = copy.ignored([
        "target/",
        "target/debug/",
        "target/kept.txt",
        "src/",
        "src/lib.rs",
        "src/new.rs",
        "build.log",
    ])?;
    let expected: HashSet<String> = ["target/debug/", "build.log"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(ignored, expected);
    assert!(copy.ignored(["src/lib.rs"])?.is_empty());
    assert!(copy.ignored(std::iter::empty())?.is_empty());
    Ok(())
}
