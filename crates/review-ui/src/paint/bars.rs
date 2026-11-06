//! The bottom bars, the `/` bar and the `?` cheat sheet.
//!
//! The lower bottom bar shows what the keys act on at the left and the current mode's keys at
//! the right, in rank order with `? keys` last at the edge.
//! Keys that cannot act now are dimmed, not removed, and a narrow window drops whole hints
//! from the highest rank down, so the bar never shifts as the selection changes.

use std::time::{Duration, Instant};

use review_core::tree::NodeKind;

use super::{Palette, Piece, Weight};
use crate::color::Hsla;
use crate::keymap::{self, Area, Section};
use crate::model::{Field, Mode, Model, plural};
use crate::scroll::ease_out;
use crate::state;

/// How long the status line's tint lasts after a failure.
pub const FLASH: Duration = Duration::from_millis(450);

/// What the review keys act on now, such as `hunk 2/3 · src/engine.rs`.
#[must_use]
pub fn target_label(model: &Model) -> String {
    let file = model.selected.clone().unwrap_or_default();
    match model.mode {
        Mode::Tree => model
            .tree_node()
            .map(|node| match node.kind {
                NodeKind::Directory => format!("{}/", node.path),
                NodeKind::File => node.path.clone(),
            })
            .unwrap_or_default(),
        Mode::Diff if model.rows.hunks.is_empty() => file,
        Mode::Diff => format!(
            "hunk {}/{} · {file}",
            model.hunk + 1,
            model.rows.hunks.len()
        ),
        Mode::Buffer => {
            let line = model
                .buffer
                .as_ref()
                .map_or(0, |buffer| buffer.cursor.row + 1);
            match model.buffer_hunk() {
                Some(hunk) => format!(
                    "hunk {}/{} · line {line} · {file}",
                    hunk + 1,
                    model.rows.hunks.len()
                ),
                None => format!("line {line} · {file}"),
            }
        }
    }
}

/// Whether the bar should offer `action` now.
#[must_use]
pub fn hint_available(model: &Model, action: &str) -> bool {
    match action {
        "NextFile" => !model.file_open() && model.next_open_file().is_some(),
        "GoToDefinition" => model.trusted,
        "ToggleApprove" | "ToggleFlag" => match model.mode {
            Mode::Buffer => model.buffer_hunk().is_some(),
            Mode::Diff => !model.rows.hunks.is_empty(),
            Mode::Tree => true,
        },
        "Dismiss" => model.mode == Mode::Buffer,
        "ToggleDiffStyle" => model.mode != Mode::Buffer,
        _ => true,
    }
}

/// A hint of the bottom bar: its keys and what they do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarHint {
    /// The keys, such as `j/k`, in the accent, or faint when they cannot act now.
    pub keys: Piece,
    /// What they do, such as `approve`.
    pub label: Piece,
}

/// How the bottom bar measures its hints, in the front end's unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HintRoom {
    /// The width the hints may take.
    pub budget: f32,
    /// The space between a hint's keys and its label.
    pub inner_gap: f32,
    /// The space between two hints.
    pub gap: f32,
}

/// The hints of the bottom bar that fit `room`, in rank order, where `width` measures a text.
///
/// When room is short, the highest-ranked hints make way whole; which hints show depends only
/// on the mode, the diff style's name and the room, so nothing moves as the selection changes.
#[must_use]
pub fn bar_hints(
    model: &Model,
    palette: &Palette,
    room: HintRoom,
    width: impl Fn(&str) -> f32,
) -> Vec<BarHint> {
    let area = model.area();
    let mut hints = keymap::hints(&model.settings.keymap, area, |action| {
        hint_available(model, action)
    });
    // While a search is in the diff, `n` steps through its matches.
    if area == Area::Diff && model.diff_search_active() {
        for hint in hints.iter_mut().filter(|hint| hint.action == "NextFile") {
            hint.label = "next match";
            hint.available = true;
        }
    }
    // `|` says which diff style shows, the one it moves on from.
    let style = model.look().style.name.clone();
    let label = |hint: &keymap::Hint| match hint.action {
        "ToggleDiffStyle" => style.clone(),
        _ => hint.label.to_owned(),
    };
    let kept = keymap::fit(
        &hints,
        room.budget,
        |hint| width(&hint.keys) + room.inner_gap + width(&label(hint)),
        room.gap,
    );
    hints
        .into_iter()
        .enumerate()
        .filter(|(index, _)| kept.contains(index))
        .map(|(_, hint)| {
            let (key_color, label_color) = if hint.available {
                (palette.accent, palette.dim)
            } else {
                (palette.faint, palette.faint)
            };
            BarHint {
                label: Piece::new(label(&hint), label_color),
                keys: Piece::new(hint.keys, key_color),
            }
        })
        .collect()
}

/// The status line: the last thing that happened, tinted for a moment after a failure, and a
/// newer revision when there is one.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// The status, in the error color after a failure.
    pub text: Piece,
    /// The line's background: the surface, or the error color fading out after a failure.
    pub background: Hsla,
    /// A newer revision, such as `revision 3 available · R switches`, on the accent.
    pub newer: Option<Notice>,
}

/// A label on a solid background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// The label.
    pub text: Piece,
    /// Its background.
    pub background: Hsla,
}

/// The status line at `now`.
#[must_use]
pub fn status(model: &Model, palette: &Palette, now: Instant) -> Status {
    let failing = model
        .failed_at
        .map(|at| 1.0 - ease_out(now.duration_since(at).as_secs_f32() / FLASH.as_secs_f32()));
    let newer = model.snapshot.as_ref().and_then(state::Snapshot::newer);
    Status {
        text: Piece::new(
            model.status.clone(),
            if model.failed_at.is_some() {
                palette.error
            } else {
                palette.dim
            },
        ),
        background: match failing {
            Some(glow) => palette.error.alpha(0.25 * glow.max(0.0)),
            None => palette.surface,
        },
        newer: newer.map(|newer| Notice {
            text: Piece::new(
                format!("revision {newer} available · R switches"),
                palette.background,
            ),
            background: palette.accent,
        }),
    }
}

/// Whether the status line's failure tint is still fading at `now`, so the front end keeps
/// drawing frames.
#[must_use]
pub fn flashing(model: &Model, now: Instant) -> bool {
    model
        .failed_at
        .is_some_and(|at| now.duration_since(at) < FLASH)
}

/// The `/` bar of the current mode while it takes typing: the tree's filter, or the diff's or
/// the buffer's find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindBar {
    /// The field the bar edits.
    pub field: Field,
    /// The `/` before the field, in the accent.
    pub slash: Piece,
    /// How many files the filter leaves, or which match of how many the find is on.
    pub count: Piece,
}

/// The `/` bar, while the current mode's takes typing.
#[must_use]
pub fn find_bar(model: &Model, palette: &Palette) -> Option<FindBar> {
    if !model.typing() {
        return None;
    }
    let (field, count) = if model.mode == Mode::Tree {
        let files = model
            .visible_tree()
            .iter()
            .filter(|&&index| {
                model
                    .tree
                    .get(index)
                    .is_some_and(|node| node.kind == NodeKind::File)
            })
            .count();
        (Field::Filter, plural(files, "file"))
    } else {
        let find = model.search_of(model.mode)?;
        let count = match find.current {
            Some(current) => format!("{}/{}", current + 1, find.matches.len()),
            None if find.matches.len() == 1 => "1 match".to_owned(),
            None => format!("{} matches", find.matches.len()),
        };
        (Field::Find, count)
    };
    Some(FindBar {
        field,
        slash: Piece::new("/", palette.accent),
        count: Piece::new(count, palette.dim),
    })
}

/// The cells a column of the cheat sheet needs: its keys and room for their help.
pub const HELP_COLUMN_CELLS: f32 = 26.0;

/// The cells of the cheat sheet's keys column.
pub const HELP_KEYS_CELLS: f32 = 11.0;

/// The most columns the cheat sheet has.
const HELP_MAX_COLUMNS: usize = 4;

/// How many columns of `column` width the cheat sheet fits in `inner` with `gap` between
/// them, in the front end's unit: as many as fit, from one to four.
#[must_use]
pub fn help_column_count(inner: f32, column: f32, gap: f32) -> usize {
    let fit = ((inner + gap) / (column + gap)).floor();
    (1..=HELP_MAX_COLUMNS)
        .rev()
        .find(|&count| fit >= crate::scroll::float(count))
        .unwrap_or(1)
}

/// The `?` cheat sheet: every bound action by section, from the keymap in effect.
#[derive(Debug, Clone, PartialEq)]
pub struct Help {
    /// `Keyboard shortcuts`, semibold.
    pub title: Piece,
    /// How to rebind and close, beside the title or under it when narrow.
    pub note: Piece,
    /// The sections, in columns.
    pub columns: Vec<Vec<HelpSection>>,
    /// The dimmed editor background over the window behind the sheet.
    pub backdrop: Hsla,
}

/// A section of the cheat sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpSection {
    /// Its title, in capitals, over a hairline.
    pub title: Piece,
    /// Its keys and what they do; help wraps rather than hiding its end.
    pub entries: Vec<HelpEntry>,
}

/// A line of the cheat sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpEntry {
    /// The keys, in the accent.
    pub keys: Piece,
    /// What they do.
    pub help: Piece,
}

/// The cheat sheet in `count` columns.
#[must_use]
pub fn help(model: &Model, palette: &Palette, count: usize) -> Help {
    let columns = keymap::columns(keymap::sheet(&model.settings.keymap), count)
        .into_iter()
        .map(|column| {
            column
                .into_iter()
                .map(|(group, entries): Section| HelpSection {
                    title: Piece::new(group.to_uppercase(), palette.dim),
                    entries: entries
                        .into_iter()
                        .map(|(keys, help)| HelpEntry {
                            keys: Piece::new(keys, palette.accent),
                            help: Piece::new(help, palette.text),
                        })
                        .collect(),
                })
                .collect()
        })
        .collect();
    Help {
        title: Piece::new("Keyboard shortcuts", palette.text).weighted(Weight::Semibold),
        note: Piece::new("[keys] in config.toml rebinds · Esc closes", palette.faint),
        columns,
        backdrop: palette.background.alpha(0.6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sheet takes as many columns as fit, never more than four or fewer than one.
    #[test]
    fn help_columns_fit_the_room() {
        // Columns of 26 with gaps of 2: four take 110.
        assert_eq!(help_column_count(110.0, 26.0, 2.0), 4);
        assert_eq!(help_column_count(109.0, 26.0, 2.0), 3);
        assert_eq!(help_column_count(26.0, 26.0, 2.0), 1);
        assert_eq!(help_column_count(10.0, 26.0, 2.0), 1);
        assert_eq!(help_column_count(10_000.0, 26.0, 2.0), 4);
    }
}
