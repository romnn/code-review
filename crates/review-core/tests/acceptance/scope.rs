//! Scope breadcrumbs: the named scopes around a unit, read from the side it changes.

use color_eyre::eyre;
use googletest::prelude::*;
use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};
use review_core::scope::breadcrumb;
use review_core::{ScopeAnchor, Side, UnitId};

/// A unit inside a method is placed in its `impl` and function; file units have no scope.
#[test]
fn units_know_their_scope() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let review = prepare(
        &DemoPaths::under(dir.path()),
        &fixture_dir(),
        DemoRequest::Fresh,
    )?;
    let history = review.history()?;
    let view = history.current_view()?;
    let mut reader = review.repo().blob_reader()?;
    let find = |needle: &str| -> eyre::Result<UnitId> {
        view.unit_ids()
            .into_iter()
            .find(|&id| {
                view.unit(id)
                    .ok()
                    .and_then(|unit| unit.chunk())
                    .is_some_and(|chunk| {
                        chunk.added.contains(needle) || chunk.removed.contains(needle)
                    })
            })
            .ok_or_else(|| eyre::eyre!("no unit with {needle}"))
    };
    let breadcrumb_of = |id, reader: &mut _| -> eyre::Result<String> {
        let scopes = view.scope(id, reader)?;
        Ok(breadcrumb(&scopes.iter().collect::<Vec<_>>()))
    };

    let loop_body = find("self.apply(entry);")?;
    assert_that!(
        breadcrumb_of(loop_body, &mut reader)?,
        eq("impl Ledger › fn apply_batch")
    );
    assert_that!(
        view.scope_anchor(loop_body)?,
        some(eq(ScopeAnchor {
            side: Side::Head,
            line: 26
        }))
    );
    // A top-level item has no enclosing scope.
    assert_that!(
        breadcrumb_of(find("pub const LIMIT")?, &mut reader)?,
        eq("")
    );
    // A file unit, such as the rename, has none either.
    let rename = view
        .unit_ids()
        .into_iter()
        .find(|&id| view.unit(id).is_ok_and(|unit| unit.chunk().is_none()))
        .ok_or_else(|| eyre::eyre!("no file unit"))?;
    assert_that!(view.scope(rename, &mut reader)?.len(), eq(0));
    Ok(())
}
