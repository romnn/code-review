//! The built-in demo pull request: deterministic, and covering every engine feature.

use std::process::Command;

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::analysis::{FileUnit, MoveKind, UnitKind};
use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};
use review_core::{Review, RevisionNumber, UnitState};

/// What the engine made of one revision, in a comparable form.
#[derive(Debug, PartialEq, Eq)]
struct Classification {
    units: Vec<String>,
    moves: Vec<String>,
}

fn classify(review: &Review, revision: RevisionNumber) -> eyre::Result<Classification> {
    let history = review.history()?;
    let view = history.view(revision)?;
    let units = view
        .unit_ids()
        .into_iter()
        .map(|id| {
            let unit = view.unit(id)?;
            let kind = match &unit.kind {
                UnitKind::Chunk(chunk) => format!("chunk -{} +{}", chunk.old.len, chunk.new.len),
                UnitKind::File(kind) => format!("{kind:?}"),
            };
            Ok(format!(
                "{id} {} {kind} formatting_only={} moved={} noise={:?} state={}",
                view.file(id)?.display_path(),
                unit.formatting_only,
                unit.moved,
                view.noise(id)?,
                view.state(id)?.name()
            ))
        })
        .collect::<eyre::Result<_>>()?;
    let moves = view
        .moves()
        .iter()
        .map(|found| {
            format!(
                "{:?} {} -> {}",
                found.kind, found.source.location.path, found.target.location.path
            )
        })
        .collect();
    Ok(Classification { units, moves })
}

fn refs(paths: &DemoPaths) -> eyre::Result<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(&paths.repo)
        .args([
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/demo",
            "refs/heads",
        ])
        .output()?;
    Ok(String::from_utf8(output.stdout)?)
}

/// Builds the demo twice in separate places: commit IDs and the engine's classification of
/// every revision are identical, so the fixture keeps catching regressions in move or formatting
/// detection.
#[test]
fn demo_is_deterministic() -> eyre::Result<()> {
    let mut builds = Vec::new();
    for _ in 0..2 {
        let root = tempfile::tempdir()?;
        let paths = DemoPaths::under(root.path());
        prepare(&paths, &fixture_dir(), DemoRequest::Fresh)?;
        let last = RevisionNumber::new(3).ok_or_else(|| eyre::eyre!("revision 3"))?;
        let review = prepare(&paths, &fixture_dir(), DemoRequest::UpTo(last))?;
        let classifications = (1..=3)
            .filter_map(RevisionNumber::new)
            .map(|revision| classify(&review, revision))
            .collect::<eyre::Result<Vec<_>>>()?;
        builds.push((refs(&paths)?, classifications));
    }
    assert_that!(builds[0].0, eq(&builds[1].0));
    assert_that!(builds[0].1, eq(&builds[1].1));
    // The commit IDs are fixed, not just equal between two runs on one machine. Update this
    // when the fixture trees change on purpose.
    assert!(
        builds[0]
            .0
            .contains("refs/demo/feature-1 08990e0fb3a2171c575605751fcc121d6de429be"),
        "{}",
        builds[0].0
    );
    Ok(())
}

/// Revision 1 covers every engine feature, and revisions 2 and 3 show carried, changed and new
/// units.
#[test]
fn demo_covers_every_feature() -> eyre::Result<()> {
    let root = tempfile::tempdir()?;
    let paths = DemoPaths::under(root.path());
    let review = prepare(&paths, &fixture_dir(), DemoRequest::Fresh)?;
    let history = review.history()?;
    let view = history.current_view()?;
    let analysis = view.analysis().ok_or_else(|| eyre::eyre!("no analysis"))?;
    let path_of = |file: usize| {
        analysis
            .files
            .get(file)
            .map_or("", |file| file.display_path())
    };

    let formatting_files: Vec<&str> = analysis
        .files
        .iter()
        .filter(|file| file.formatting_only)
        .map(review_core::analysis::FileChange::display_path)
        .collect();
    assert_that!(
        formatting_files,
        unordered_elements_are![eq(&"config/settings.json"), eq(&"src/parser.rs")]
    );
    let file_units: Vec<FileUnit> = analysis
        .units
        .iter()
        .filter_map(|unit| match unit.kind {
            UnitKind::File(kind) => Some(kind),
            UnitKind::Chunk(_) => None,
        })
        .collect();
    for kind in [
        FileUnit::Rename,
        FileUnit::Mode,
        FileUnit::Symlink,
        FileUnit::Binary,
    ] {
        assert!(
            file_units.contains(&kind),
            "{kind:?} missing from {file_units:?}"
        );
    }
    let moves: Vec<MoveKind> = analysis.moves.iter().map(|found| found.kind).collect();
    assert_that!(
        moves,
        unordered_elements_are![eq(&MoveKind::Moved), eq(&MoveKind::MovedWithEdits)]
    );
    assert!(
        analysis
            .units
            .iter()
            .any(|unit| unit.chunk().is_some_and(|chunk| chunk.new.len == 0)),
        "no deletion-only chunk"
    );
    assert!(
        analysis.files.iter().any(|file| file.noise.is_some()),
        "no noise"
    );
    let highlighted = analysis
        .units
        .iter()
        .filter(|unit| path_of(unit.file) == "src/engine.rs")
        .filter_map(|unit| unit.chunk())
        .any(|chunk| chunk.added_highlights.len() == 1 && chunk.added.contains("250"));
    assert!(highlighted, "the engine's real edit is not highlighted");
    assert!(history.warnings().is_empty(), "{:?}", history.warnings());

    // Revision 2: carried, changed and new units.
    let review = prepare(
        &paths,
        &fixture_dir(),
        DemoRequest::UpTo(RevisionNumber::new(2).ok_or_else(|| eyre::eyre!("2"))?),
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let states: Vec<UnitState> = view
        .unit_ids()
        .into_iter()
        .map(|id| view.state(id))
        .collect::<Result<_, _>>()?;
    assert!(states.iter().any(|state| matches!(
        state,
        UnitState::Reviewed {
            inherited: true,
            ..
        }
    )));
    assert!(
        states
            .iter()
            .any(|state| matches!(state, UnitState::ChangedSinceReviewed { .. }))
    );
    assert_that!(view.number().get(), eq(2));
    Ok(())
}
