//! The headers over the diff and the buffer, the progress meter, and the diff's loading, empty
//! and error states.

use std::time::Instant;

use super::display::display_width;
use super::{Bar, Palette, Piece, Room, Weight, split_path};
use crate::config::DiffLayout;
use crate::model::buffer::Origin;
use crate::model::{Model, Phase};
use crate::rows::FileSection;
use crate::scroll::float;
use crate::state;

/// The cells of a progress meter's bar.
pub const METER_BAR_CELLS: f32 = 5.0;

/// The most cells of a file header's path that count against its room; past that, the path
/// truncates before the meter and counts make way.
const PATH_CELLS: f32 = 16.0;

/// A reviewed/total meter: a short bar and the counts, padded to the width of `total/total` so
/// it never shifts as numbers change.
#[derive(Debug, Clone, PartialEq)]
pub struct Meter {
    /// The bar, [`METER_BAR_CELLS`] cells wide.
    pub bar: Bar,
    /// The counts, `reviewed/total`, padded on the left.
    pub counts: Piece,
}

/// The meter of `reviewed` of `total`.
#[must_use]
pub fn meter(palette: &Palette, reviewed: usize, total: usize) -> Meter {
    let fraction = if total == 0 {
        0.0
    } else {
        float(reviewed) / float(total)
    };
    let counts = format!("{total}/{total}").len();
    let fill = if reviewed == total && total > 0 {
        palette.accepted
    } else {
        palette.accent
    };
    Meter {
        bar: Bar {
            fraction,
            fill,
            auto: 0.0,
            auto_fill: fill,
            track: palette.hairline,
        },
        counts: Piece::new(
            format!("{:>counts$}", format!("{reviewed}/{total}")),
            palette.dim,
        ),
    }
}

/// The width a meter of `total` takes with cells `cell` wide and `gap` between its bar and its
/// counts, in the caller's unit; the bar's width is rounded to whole units.
#[must_use]
pub fn meter_width(total: usize, cell: f32, gap: f32) -> f32 {
    let counts = format!("{total}/{total}").len();
    (cell * METER_BAR_CELLS).round() + gap + float(counts) * cell
}

/// The selected file's header: its change kind, path and details, and its line counts and
/// progress where they fit.
#[derive(Debug, Clone, PartialEq)]
pub struct FileHeader {
    /// The change kind's letter, in bold and colored by kind.
    pub kind: Option<Piece>,
    /// The path's directory, with its final `/`; it truncates from its start.
    pub dir: Piece,
    /// The file name, in bold.
    pub name: Piece,
    /// A rename, entry changes, what kind of change it is, and why side by side shows
    /// unified, joined with ` · `.
    pub details: Piece,
    /// The file's length, such as `142 lines`, before the line counts, when it fits.
    pub lines: Option<Piece>,
    /// The added and removed line counts, `+12` and `−3`, when they fit.
    pub counts: Option<[Piece; 2]>,
    /// The hunks approved of the file's, when it fits.
    pub meter: Option<Meter>,
}

/// The selected file's header, with the parts that fit `room`, or `None` with no file
/// selected; [`NO_FILE`] says so in [`Palette::faint`].
///
/// When the room is narrow, the file's length makes way first, then the line counts, then the
/// meter; the path keeps some room and truncates from its start.
/// `meter_gap` is the gap inside the meter, between its bar and its counts.
#[must_use]
pub fn file_header(
    model: &Model,
    palette: &Palette,
    room: Room,
    meter_gap: f32,
) -> Option<FileHeader> {
    let section = model.rows.files.first()?;
    let (dir, name) = split_path(&section.path);
    let change = section.change.as_ref();
    let units: Vec<_> = model.rows.hunks.concat();
    let approved = model
        .view()
        .map(|view| {
            state::badges(&view, &units, &model.pending)
                .into_iter()
                .filter(|&badge| state::is_approved(badge))
                .count()
        })
        .unwrap_or(0);
    let Room { budget, cell, gap } = room;
    let text = |text: &str| float(display_width(text)) * cell;
    let added = format!("+{}", section.added);
    let removed = format!("−{}", section.removed);
    let lead = change.map_or(0.0, |_| cell + gap) + text(&section.path).min(cell * PATH_CELLS);
    let meter_room = gap + meter_width(units.len(), cell, meter_gap);
    let counts_room = gap + text(&added) + gap + text(&removed);
    let show_meter = lead + meter_room <= budget;
    let show_counts = show_meter && lead + meter_room + counts_room <= budget;
    let lines = section.lines.map(|lines| match lines {
        1 => "1 line".to_owned(),
        lines => format!("{lines} lines"),
    });
    let lines = lines.filter(|lines| {
        show_counts && lead + meter_room + counts_room + gap + text(lines) <= budget
    });
    Some(FileHeader {
        kind: change.map(|change| {
            Piece::new(
                change.kind.letter().to_string(),
                palette.tone(change.tone()),
            )
            .weighted(Weight::Bold)
        }),
        dir: Piece::new(dir, palette.dim),
        name: Piece::new(name, palette.text).weighted(Weight::Bold),
        details: Piece::new(file_details(model, section).join(" · "), palette.faint),
        lines: lines.map(|lines| Piece::new(lines, palette.faint)),
        counts: show_counts.then(|| {
            [
                Piece::new(added, palette.added_sign),
                Piece::new(removed, palette.removed_sign),
            ]
        }),
        meter: show_meter.then(|| meter(palette, approved, units.len())),
    })
}

/// What the file header says with no file selected.
pub const NO_FILE: &str = "No file selected";

/// What the file header says about `section` after its path: a rename, entry changes, what
/// kind of change it is, and why side by side shows unified.
fn file_details(model: &Model, section: &FileSection) -> Vec<String> {
    let mut details: Vec<String> = Vec::new();
    if let Some(old) = &section.old_path {
        details.push(format!("renamed from {old}"));
    }
    if let Some(change) = &section.change {
        details.extend(
            change
                .entry_changes
                .iter()
                .map(|unit| format!("{unit:?}").to_lowercase()),
        );
        if change.formatting_only {
            details.push("reformatted only".to_owned());
        }
        if let Some(noise) = change.noise {
            details.push(noise.name().to_owned());
        }
        if change.moves {
            details.push("moved code".to_owned());
        }
    }
    if model.look().layout == DiffLayout::SideBySide && !model.side_by_side() {
        details.push("unified · too narrow for side by side".to_owned());
    }
    details
}

/// The buffer's header: the file and the scopes around the cursor, where the text comes from,
/// and the cursor's place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferHeader {
    /// The file's path, in bold.
    pub title: Piece,
    /// The scopes around the cursor, after ` › `, so they stay in view as the buffer scrolls.
    pub scope: Option<Piece>,
    /// Where the text comes from, such as `whole file, read-only`.
    pub origin: Piece,
    /// The cursor's line and column, such as `12:5`.
    pub cursor: Piece,
}

/// The open buffer's header.
#[must_use]
pub fn buffer_header(model: &Model, palette: &Palette) -> Option<BufferHeader> {
    let buffer = model.buffer.as_ref()?;
    let origin = match &buffer.origin {
        Origin::Review(_) => "whole file, read-only",
        Origin::Definition(_) => "definition, read-only · Ctrl+O back",
        Origin::Head(_) => "not changed in this review, read-only",
    };
    Some(BufferHeader {
        title: Piece::new(buffer.title(), palette.text).weighted(Weight::Bold),
        scope: buffer
            .scope()
            .map(|scope| Piece::new(format!(" › {scope}"), palette.dim)),
        origin: Piece::new(origin, palette.faint),
        cursor: Piece::new(
            format!("{}:{}", buffer.cursor.row + 1, buffer.cursor.col + 1),
            palette.dim,
        ),
    })
}

/// A card in the middle of the diff for its loading, empty and error states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateCard {
    /// What is going on, semibold.
    pub title: Piece,
    /// More about it.
    pub detail: Piece,
}

/// The diff's loading, error or empty state at `now`, when it is in one.
#[must_use]
pub fn diff_state(model: &Model, palette: &Palette, now: Instant) -> Option<StateCard> {
    let card = |title: &str, detail: String, color| StateCard {
        title: Piece::new(title, palette.text).weighted(Weight::Semibold),
        detail: Piece::new(detail, color),
    };
    match &model.phase {
        Phase::Loading(started) => Some(card(
            "Opening the review",
            format!("Reading history and analysis{}", dots(*started, now)),
            palette.dim,
        )),
        Phase::Failed(err) => Some(card(
            "Couldn't open this review",
            err.clone(),
            palette.error,
        )),
        // A previewed file's diff follows once the tree cursor rests.
        Phase::Ready if model.preview.is_some() => None,
        Phase::Ready if model.rows.rows.is_empty() => Some(card(
            "Nothing to show",
            "Pick a file on the left.".to_owned(),
            palette.dim,
        )),
        Phase::Ready => None,
    }
}

/// Animated dots for a loading state that started at `started`.
fn dots(started: Instant, now: Instant) -> &'static str {
    match (now.duration_since(started).as_millis() / 400) % 4 {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The meter's counts are padded to the widest they get, so its width never changes as
    /// hunks are approved.
    #[test]
    fn meters_keep_their_width() {
        assert_eq!(meter_width(12, 10.0, 8.0), 50.0 + 8.0 + 50.0);
        assert_eq!(meter_width(12, 1.0, 1.0), 5.0 + 1.0 + 5.0);
    }

    /// The loading dots grow every 400 ms and start over after three.
    #[test]
    fn loading_dots_cycle() {
        let started = Instant::now();
        let at = |millis| dots(started, started + Duration::from_millis(millis));
        assert_eq!(at(0), "");
        assert_eq!(at(400), ".");
        assert_eq!(at(1200), "...");
        assert_eq!(at(1600), "");
    }
}
