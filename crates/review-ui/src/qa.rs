//! QA mode's `state` answer: what the review shows, which every front end reports alike, with
//! where it is on screen, which each front end measures in its own units.
//!
//! [`state`] writes the shared part from the model and asks the front end's [`Screen`] for its
//! geometry at the places it belongs, so the keys of the answer keep one order in every front
//! end and a script's expectations read the same apart from the numbers of the geometry:
//! review-gui reports logical pixels, review-tui terminal cells.

use std::ops::Range;
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::model::{Model, Phase};
use crate::paint::{Palette, bars, files};
use crate::rows::{Body, Cell};
use crate::state::{self, Progress};

/// Where things are on a front end's screen, in its own units, for QA mode's `state`.
pub trait Screen {
    /// The diff rows on screen, from the first one any of shows to the end of the last.
    fn rows(&self) -> Range<usize>;

    /// Where diff row `row` is on screen.
    fn row_y(&self, row: usize) -> Value;

    /// Where the tree row at `position` among the visible ones is on screen.
    fn tree_y(&self, position: usize) -> Value;

    /// The diff list's geometry, reported after the rows: `visible_rows`, `scroll_offset`,
    /// `row_height` and `viewport`.
    fn list(&self) -> Map<String, Value>;

    /// The window's geometry, reported after `trusted`, such as `layout` with the files pane's
    /// width while it shows.
    fn window(&self) -> Map<String, Value>;

    /// The open settings panel's geometry.
    fn settings(&self) -> Value;

    /// The text's geometry, reported after `view`, such as `metrics`.
    fn text(&self) -> Map<String, Value>;
}

/// The observable state of `model` on `screen`, as one JSON object.
#[must_use]
pub fn state(model: &Model, screen: &dyn Screen) -> Value {
    let mut answer = Map::new();
    put_review(&mut answer, model);
    answer.insert(
        "rows".to_owned(),
        Value::Array(diff_rows(model, screen, screen.rows())),
    );
    answer.extend(screen.list());
    put_panes(&mut answer, model, screen);
    answer.extend(screen.window());
    answer.insert("view".to_owned(), view_state(model, screen));
    answer.extend(screen.text());
    answer.insert("status".to_owned(), json!(model.status));
    answer.insert(
        "progress".to_owned(),
        json!({
            "files": model.file_totals(),
            "hunks": model.totals(),
            "auto_approved": model.auto_approved(),
        }),
    );
    let palette = Palette::of(model);
    let header = files::files_header(model, &palette);
    let review = header
        .number
        .iter()
        .map(|number| number.text.as_str())
        .chain([header.name.text.as_str()])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    answer.insert("files_header".to_owned(), json!(review));
    // The bottom bar's left side, whole, as a bar with room for all of it shows it.
    let sync: String = bars::sync_label(model, &palette, SystemTime::now(), f32::MAX, |text| {
        crate::scroll::float(text.chars().count())
    })
    .into_iter()
    .map(|piece| piece.text)
    .collect();
    answer.insert("sync".to_owned(), json!(sync));
    Value::Object(answer)
}

/// Where the review is, the file shown and its selected hunk, and the marks under way.
fn put_review(answer: &mut Map<String, Value>, model: &Model) {
    let view = model.view();
    let hunk_units = model.hunk_units();
    let hunk_badges: Vec<String> = view
        .as_ref()
        .map(|view| {
            state::badges(view, &hunk_units, &model.pending)
                .into_iter()
                .map(|badge| format!("{badge:?}"))
                .collect()
        })
        .unwrap_or_default();
    let snapshot = model.snapshot.as_ref();
    let shown = model.rows.files.first();
    let entries = [
        (
            "phase",
            json!(match &model.phase {
                Phase::Loading(_) => "loading".to_owned(),
                Phase::Ready => "ready".to_owned(),
                Phase::Failed(err) => format!("failed: {err}"),
            }),
        ),
        (
            "revision",
            json!(snapshot.map(|snapshot| snapshot.revision.to_string())),
        ),
        (
            "newer",
            json!(
                snapshot
                    .and_then(state::Snapshot::newer)
                    .map(|revision| revision.to_string())
            ),
        ),
        ("mode", json!(format!("{:?}", model.mode))),
        ("area", json!(format!("{:?}", model.area()))),
        ("screen", json!(format!("{:?}", model.screen))),
        ("selected", json!(model.selected)),
        ("shown", json!(shown.map(|section| section.path.clone()))),
        ("preview", json!(model.preview.is_some())),
        (
            "highlighted",
            json!(
                shown
                    .and_then(|section| model.folds.syntax.get(&section.path))
                    .is_some_and(|syntax| syntax.head.is_some() || syntax.base.is_some())
            ),
        ),
        ("hunk", json!(model.hunk)),
        ("hunks", json!(model.rows.hunks.len())),
        (
            "hunk_units",
            json!(
                hunk_units
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            ),
        ),
        ("hunk_badges", json!(hunk_badges)),
        (
            "hunk_rows",
            json!(
                model
                    .hunk_rows
                    .get(model.hunk)
                    .map(|rows| [rows.start, rows.end])
            ),
        ),
        ("pending", json!(model.pending.len())),
        (
            "queued",
            json!(model.marks.len() + usize::from(model.marking)),
        ),
        ("row_count", json!(model.rows.rows.len())),
    ];
    answer.extend(entries.map(|(key, value)| (key.to_owned(), value)));
}

/// The tree, the buffer, the searches, the picker, the selection and the trust.
fn put_panes(answer: &mut Map<String, Value>, model: &Model, screen: &dyn Screen) {
    let mut collapsed: Vec<&str> = model.tree_collapsed.iter().map(String::as_str).collect();
    collapsed.sort_unstable();
    let entries = [
        ("tree", Value::Array(tree_rows(model, screen))),
        ("tree_cursor", json!(model.tree_cursor)),
        ("collapsed", json!(collapsed)),
        ("buffer", buffer(model)),
        ("find", find(model)),
        ("filter", filter(model)),
        ("picker", picker(model)),
        ("selection", selection(model)),
        ("selected_text", json!(model.selected_text())),
        ("trusted", json!(model.trusted)),
    ];
    answer.extend(entries.map(|(key, value)| (key.to_owned(), value)));
}

/// The diff rows in `range`: where each comes from, what it is and where it is.
fn diff_rows(model: &Model, screen: &dyn Screen, range: Range<usize>) -> Vec<Value> {
    range
        .filter_map(|index| {
            let row = model.rows.rows.get(index)?;
            // Where a row's code is in each file: one line across the width, or both sides.
            let code = match &row.body {
                Body::Code(cell) => json!({
                    "head": cell.head.as_ref().map(|head| head.line),
                    "base": cell.base,
                }),
                Body::Pair(pair) => json!({
                    "left": side(pair.left.as_ref()),
                    "right": side(pair.right.as_ref()),
                }),
                Body::Note(_) => Value::Null,
            };
            Some(json!({
                "row": index,
                "source": row.source,
                "kind": format!("{:?}", row.kind()),
                "hunk": row.hunk,
                "y": screen.row_y(index),
                "text": row.text().chars().take(80).collect::<String>(),
                "tag": row.note().and_then(|note| note.tag.clone()),
                "scope": row.note().and_then(|note| note.scope.clone()),
                "code": code,
            }))
        })
        .collect()
}

/// A side of a pair: its kind, both positions and its text, or `null` for padding.
fn side(cell: Option<&Cell>) -> Value {
    cell.map_or(Value::Null, |cell| {
        json!({
            "kind": format!("{:?}", cell.kind),
            "head": cell.head.as_ref().map(|head| head.line),
            "base": cell.base,
            "text": cell.text.chars().take(60).collect::<String>(),
        })
    })
}

/// The visible tree rows, each with its change kind, its progress and where it is.
fn tree_rows(model: &Model, screen: &dyn Screen) -> Vec<Value> {
    model
        .visible_tree()
        .iter()
        .enumerate()
        .filter_map(|(position, &index)| {
            let node = model.tree.get(index)?;
            Some(json!({
                "path": node.path,
                "kind": format!("{:?}", node.kind),
                "change": node.change.as_ref().map(|change| change.kind.letter().to_string()),
                "progress": format!("{:?}", Progress::of(&node.counts)),
                "y": screen.tree_y(position),
            }))
        })
        .collect()
}

/// The file buffer: its title, the scopes and line at its cursor and the hunk there.
fn buffer(model: &Model) -> Value {
    json!(model.buffer.as_ref().map(|buffer| {
        json!({
            "title": buffer.title(),
            "scope": buffer.scope(),
            "row": buffer.cursor.row,
            "col": buffer.cursor.col,
            "line": buffer.line(buffer.cursor.row).chars().take(80).collect::<String>(),
            "hunk": model.buffer_hunk(),
            "back": buffer.previous.is_some(),
        })
    }))
}

/// The `/` search of the diff or the buffer.
fn find(model: &Model) -> Value {
    json!(model.find.as_ref().map(|find| {
        let current = find.current.and_then(|index| find.matches.get(index));
        json!({
            "target": format!("{:?}", find.target),
            "query": find.query,
            "matches": find.matches.len(),
            "current": find.current,
            "current_column": current.map(|found| format!("{:?}", found.column)),
            "editing": find.editing,
        })
    }))
}

/// The tree's `/` filter.
fn filter(model: &Model) -> Value {
    json!(model.filter.as_ref().map(|filter| {
        json!({
            "query": filter.query,
            "editing": filter.editing,
        })
    }))
}

/// The open picker and its first entries.
fn picker(model: &Model) -> Value {
    json!(model.picker.as_ref().map(|picker| {
        let entries: Vec<Value> = picker
            .entries
            .iter()
            .take(8)
            .map(|entry| {
                json!({
                    "path": entry.path,
                    "line": entry.line,
                    "text": entry.text.chars().take(80).collect::<String>(),
                    "changed": entry.change.is_some(),
                })
            })
            .collect();
        json!({
            "kind": format!("{:?}", picker.kind),
            "scope": format!("{:?}", picker.scope),
            "query": picker.query,
            "editing": picker.editing,
            "searching": picker.searching,
            "count": picker.entries.len(),
            "truncated": picker.truncated,
            "selected": picker.selected,
            "entries": entries,
        })
    }))
}

/// The text selection in the diff or the buffer.
fn selection(model: &Model) -> Value {
    json!(model.selection.map(|selection| {
        json!({
            "pane": format!("{:?}", selection.pane),
            "column": format!("{:?}", selection.column),
            "anchor": [selection.anchor.row, selection.anchor.col],
            "head": [selection.head.row, selection.head.col],
        })
    }))
}

/// The look on screen and the settings list.
fn view_state(model: &Model, screen: &dyn Screen) -> Value {
    let look = model.look();
    json!({
        "overlay": model.overlay.map(|overlay| format!("{overlay:?}")),
        "token_emphasis": look.token_emphasis,
        "diff_style": look.style.name,
        "theme": look.theme.name,
        "layout": look.layout.name(),
        "effective_layout": model.rows.presentation.layout.name(),
        "density": look.density.name(),
        "effective_density": model.rows.presentation.density.name(),
        "approved": model.folds.approved.name(),
        "style": {
            "syntax": look.style.syntax,
            "background": look.style.background.name(),
            "headers": look.style.headers.name(),
            "change_bar": look.style.change_bar,
        },
        "applied_theme": model.settings.appearance.applied.theme.name,
        "previewing": model.previewing(),
        "settings": settings(model, screen),
    })
}

/// The open settings list: its geometry, its search, the entry under its cursor and how many
/// entries show.
fn settings(model: &Model, screen: &dyn Screen) -> Value {
    let Some(panel) = &model.settings_panel else {
        return Value::Null;
    };
    let shown = model.shown_entries();
    json!({
        "geometry": screen.settings(),
        "query": panel.query,
        "searching": panel.searching,
        "cursor": shown
            .iter()
            .find(|entry| entry.choice == panel.cursor)
            .map(|entry| format!("{}: {}", entry.section, entry.label)),
        "entries": shown.len(),
    })
}
