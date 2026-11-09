//! View state: each front end keeps how it last showed a review beside the review, outside its
//! event log.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::Error;
use serde::{Deserialize, Serialize};

use crate::common::{TestRepo, refresh};

/// A front end's view state, as a front end would define it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Folders {
    collapsed: Vec<String>,
}

/// A repository with one change on `feature` and its recorded review.
fn reviewed_repo() -> eyre::Result<TestRepo> {
    let repo = TestRepo::new()?;
    repo.write("src/a.txt", "one\n")?;
    let base = repo.commit("base")?;
    repo.branch_at("feature", &base)?;
    repo.write("src/a.txt", "two\n")?;
    repo.commit("head")?;
    refresh(&repo.review()?)?;
    Ok(repo)
}

/// A saved state reads back through a fresh handle, each front end keeps its own, and a save
/// replaces the previous state without touching the event log.
#[test]
fn view_state_round_trips_per_front_end() -> eyre::Result<()> {
    let repo = reviewed_repo()?;
    let review = repo.review()?;
    assert_that!(review.view_state::<Folders>("review-lite")?, none());
    let events = std::fs::read(review.dir().events())?;

    // Two front ends save different states.
    let lite = Folders {
        collapsed: vec!["src".to_owned()],
    };
    review.save_view_state("review-lite", &lite)?;
    review.save_view_state(
        "review-tui",
        &Folders {
            collapsed: Vec::new(),
        },
    )?;

    // A fresh handle reads each back unchanged.
    let reopened = repo.review()?;
    assert_that!(
        reopened.view_state::<Folders>("review-lite")?,
        some(eq(&lite))
    );
    assert_that!(
        reopened.view_state::<Folders>("review-tui")?,
        some(field!(Folders.collapsed, is_empty()))
    );

    // A later save replaces the earlier one whole.
    let unfolded = Folders {
        collapsed: Vec::new(),
    };
    reopened.save_view_state("review-lite", &unfolded)?;
    assert_that!(
        review.view_state::<Folders>("review-lite")?,
        some(eq(&unfolded))
    );

    // View state is not review state: the event log and the history are untouched.
    assert_that!(std::fs::read(review.dir().events())?, eq(&events));
    assert_that!(review.history()?.revisions().len(), eq(1));
    Ok(())
}

/// A front-end name that is not one plain lowercase file name is refused before anything is
/// read or written, so no name can reach outside the review's `view` directory.
#[test]
fn view_state_refuses_names_that_are_not_plain() -> eyre::Result<()> {
    let repo = reviewed_repo()?;
    let review = repo.review()?;
    let state = Folders {
        collapsed: Vec::new(),
    };
    let too_long = "a".repeat(65);
    for name in [
        "",
        "../events",
        "a/b",
        "lite.json",
        "Lite",
        "lite ",
        too_long.as_str(),
    ] {
        assert_that!(
            review.save_view_state(name, &state),
            err(matches_pattern!(Error::InvalidFrontEnd(eq(name)))),
            "saving under {name:?}"
        );
        assert_that!(
            review.view_state::<Folders>(name),
            err(matches_pattern!(Error::InvalidFrontEnd(eq(name)))),
            "reading under {name:?}"
        );
    }
    assert!(!review.dir().root().join("view").exists());
    // The longest plain name is accepted.
    review.save_view_state(&"a".repeat(64), &state)?;
    Ok(())
}

/// A state saved in another format reads as a decode error naming the file, which a front end
/// can treat as having no state.
#[test]
fn view_state_in_another_format_is_a_decode_error() -> eyre::Result<()> {
    let repo = reviewed_repo()?;
    let review = repo.review()?;
    review.save_view_state("review-lite", &["src"])?;
    let path = review.dir().view_state("review-lite")?;
    assert_that!(
        review.view_state::<Folders>("review-lite"),
        err(matches_pattern!(Error::Decode {
            path: eq(&path),
            ..
        }))
    );
    Ok(())
}
