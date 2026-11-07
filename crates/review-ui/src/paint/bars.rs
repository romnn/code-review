//! The bottom bars, the `/` bar and the `?` cheat sheet.
//!
//! The lower bottom bar shows which revision shows and how fresh it is at the left and the
//! current mode's keys at the right, in rank order with `? keys` last at the edge.
//! Keys that cannot act now are dimmed, not removed, and a narrow window drops whole hints
//! from the highest rank down, so the bar never shifts as the selection changes.

use std::time::{Duration, Instant, SystemTime};

use review_core::RefreshStep;
use review_core::tree::NodeKind;

use super::{Palette, Piece, Weight};
use crate::color::Hsla;
use crate::keymap::{self, Area, Section};
use crate::model::sync::ago;
use crate::model::{Field, Mode, Model, Phase, SyncStep, plural};
use crate::scroll::{self, ease_out};
use crate::state;

/// How long the status line's tint lasts after a failure.
pub const FLASH: Duration = Duration::from_millis(450);

/// The braille spinner's frames, a tenth of a second each.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What the bottom bar's left side says: which revision shows and how fresh it is.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncPart {
    /// What it says.
    piece: Piece,
    /// The first to make way when room runs out is the highest; 0 never does.
    rank: u8,
}

/// The bottom bar's left side at `now`, with the parts that fit `budget` when `width` measures
/// a text, and ` · ` between them: the revision on screen and its head commit, a newer
/// revision waiting, and when the review last synced, or what a running fetch is doing, or
/// for a moment that it failed.
///
/// When room runs out, the commit makes way first, then the time since the sync, then the
/// newer revision; the revision and what a fetch is doing stay.
#[must_use]
pub fn sync_label(
    model: &Model,
    palette: &Palette,
    now: SystemTime,
    budget: f32,
    width: impl Fn(&str) -> f32,
) -> Vec<Piece> {
    let mut parts = Vec::new();
    let part = |piece: Piece, rank: u8| SyncPart { piece, rank };
    if let Some(snapshot) = &model.snapshot {
        parts.push(part(
            Piece::new(format!("rev {}", snapshot.revision), palette.text),
            0,
        ));
        if let Some(record) = snapshot.view().and_then(|view| view.record()) {
            // Seven digits, as git abbreviates a commit.
            let head = record.head.as_str().get(..7).unwrap_or_default();
            parts.push(part(Piece::new(head, palette.faint), 3));
        }
        if let Some(newer) = snapshot.newer() {
            parts.push(part(
                Piece::new(format!("rev {newer} available"), palette.accent),
                1,
            ));
        }
    }
    let spinner = SPINNER.get(spin_frame(now)).copied().unwrap_or_default();
    let activity = match (&model.phase, model.sync.step()) {
        (Phase::Loading(_), _) => Some(format!("{spinner} opening…")),
        (_, Some(step)) => Some(format!("{spinner} {}", step_label(model, step))),
        (_, None) => None,
    };
    if let Some(activity) = activity {
        parts.push(part(Piece::new(activity, palette.dim), 0));
    } else if model.sync.failing(now) {
        parts.push(part(Piece::new("sync failed", palette.error), 0));
    } else if let Some(synced) = &model.sync.synced {
        parts.push(part(
            Piece::new(format!("synced {}", ago(synced, now)), palette.faint),
            2,
        ));
    }
    let separator = " · ";
    let total = |parts: &[SyncPart]| {
        parts
            .iter()
            .map(|part| width(&part.piece.text))
            .sum::<f32>()
            + width(separator) * scroll::float(parts.len().saturating_sub(1))
    };
    while total(&parts) > budget {
        let Some(position) = parts
            .iter()
            .enumerate()
            .filter(|(_, part)| part.rank > 0)
            .max_by_key(|(_, part)| part.rank)
            .map(|(position, _)| position)
        else {
            break;
        };
        parts.remove(position);
    }
    let mut pieces = Vec::new();
    for (index, SyncPart { piece, .. }) in parts.into_iter().enumerate() {
        if index > 0 {
            pieces.push(Piece::new(separator, palette.faint));
        }
        pieces.push(piece);
    }
    pieces
}

/// The spinner's frame at `now`, which turns every tenth of a second.
fn spin_frame(now: SystemTime) -> usize {
    let tenths = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() / 100);
    usize::try_from(tenths % 10).unwrap_or(0)
}

/// What a running fetch's `step` is doing, as the bar says it.
fn step_label(model: &Model, step: SyncStep) -> String {
    match step {
        SyncStep::Refresh(RefreshStep::Fetching) => "syncing…".to_owned(),
        SyncStep::Refresh(RefreshStep::Analyzing) => {
            // A new revision is the one after the newest recorded.
            let next = model
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.history.current())
                .and_then(review_core::RevisionNumber::next);
            match next {
                Some(next) => format!("analyzing rev {next}…"),
                None => "analyzing…".to_owned(),
            }
        }
        SyncStep::Refresh(RefreshStep::Describing) => "reading the pull request…".to_owned(),
        SyncStep::Carrying => "carrying marks…".to_owned(),
    }
}

/// Whether the bar should offer `action` now.
#[must_use]
pub fn hint_available(model: &Model, action: &str) -> bool {
    match action {
        "NextFile" => model.adjacent_file(true).is_some() || model.adjacent_file(false).is_some(),
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
    // While a search is in the diff, `n` and `N` step through its matches.
    if area == Area::Diff && model.diff_search_active() {
        for hint in hints.iter_mut().filter(|hint| hint.action == "NextFile") {
            hint.label = "match";
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
