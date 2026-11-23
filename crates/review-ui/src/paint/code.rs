//! Lines of code: their styled runs (syntax colors, emphasized tokens, find matches, the
//! selection and the cursor), how the diff style inks them, their gutters, and the bar along a
//! hunk's rows.

use std::ops::Range;
use std::sync::Arc;

use super::palette::HunkTone;
use super::{Layer, Palette, Piece, Run, Style};
use crate::color::Hsla;
use crate::config::Background;
use crate::find::Match;
use crate::model::buffer::Marker;
use crate::model::{Mode, Model};
use crate::rows::{Cell, FullText, RowKind};
use crate::syntax::{SideSyntax, Span, Tone};
use crate::text::{Column, Pane, Selection, TextPos};
use crate::theme::{self, Theme};

/// Where a diff row is relative to the hunks, for its band and the bar along its left edge,
/// alike in either view, so the code reads as one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// Outside every hunk's rows: no band and no bar.
    Outside,
    /// In a hunk's rows: every hunk has a bar in its tone's color, the selected one's solid
    /// and the others' quieter, so whether code still needs the reviewer shows whatever its
    /// lines look like.
    Hunk {
        /// What the hunk asks of the reviewer.
        tone: HunkTone,
        /// Whether it is the selected hunk.
        selected: bool,
    },
}

impl Frame {
    /// The bar along the row's left edge: the selected hunk's solid color, another hunk's
    /// quieter one, or none outside the hunks.
    #[must_use]
    pub fn bar(self, palette: &Palette) -> Option<Hsla> {
        match self {
            Self::Outside => None,
            Self::Hunk {
                tone,
                selected: true,
            } => Some(palette.hunk_solid(tone)),
            Self::Hunk {
                tone,
                selected: false,
            } => Some(palette.hunk_solid(tone).alpha(UNSELECTED_BAR_ALPHA)),
        }
    }

    /// The band behind a hunk's rows; a changed line's own background covers it.
    ///
    /// Only approved hunks have one, since their code has lost its diff colors: what the
    /// reviewer approved stands apart from the code around it, the selected one a step
    /// brighter, and everything else is as plain as the file's code, the bars marking the
    /// hunks.
    #[must_use]
    pub fn band(self, palette: &Palette) -> Option<Hsla> {
        match self {
            Self::Hunk {
                tone: HunkTone::Approved,
                selected,
            } => Some(if selected {
                palette.selected_band
            } else {
                palette.band
            }),
            _ => None,
        }
    }

    /// Whether the row is in the selected hunk.
    #[must_use]
    pub fn selected(self) -> bool {
        matches!(self, Self::Hunk { selected: true, .. })
    }

    /// How much of the frame's width the bar takes: all of it in the selected hunk, and a
    /// sliver in the others, so the selected hunk shows at a glance.
    #[must_use]
    pub fn bar_share(self) -> f32 {
        if self.selected() { 1.0 } else { THIN_BAR_SHARE }
    }
}

/// The share of the frame's width the bar of a hunk that is not selected takes.
const THIN_BAR_SHARE: f32 = 0.67;

/// How strongly the bar of a hunk that is not selected shows.
const UNSELECTED_BAR_ALPHA: f32 = 0.8;

/// Where diff row `index` is relative to the hunks: in the selected one only while the diff
/// has the keys.
#[must_use]
pub fn frame(model: &Model, index: usize) -> Frame {
    let selected = (model.mode == Mode::Diff).then_some(model.hunk);
    match hunk_at(model, index) {
        Some(hunk) => Frame::Hunk {
            tone: hunk_tone(model, hunk),
            selected: Some(hunk) == selected,
        },
        None => Frame::Outside,
    }
}

/// The tone of hunk `hunk`, from its units' review states and the marks still saving.
#[must_use]
pub fn hunk_tone(model: &Model, hunk: usize) -> HunkTone {
    let units = model.rows.hunks.get(hunk).map_or(&[][..], Vec::as_slice);
    let badges = model
        .view()
        .map(|view| crate::state::badges(&view, units, &model.pending))
        .unwrap_or_default();
    HunkTone::of(&badges)
}

/// The hunk that diff row `index` belongs to for its band and bar: from the hunk's first row
/// through its last, its changed lines and, while it is folded, its header.
fn hunk_at(model: &Model, index: usize) -> Option<usize> {
    // Hunks are in row order, so the one that may hold the row is the last starting at or
    // before it.
    let hunk = model
        .hunk_rows
        .partition_point(|rows| rows.start <= index)
        .checked_sub(1)?;
    model.hunk_rows.get(hunk)?.contains(&index).then_some(hunk)
}

/// A hunk marked on the diff's scrollbar track, its rows as shares of the diff's from the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RulerMark {
    /// Where the hunk's first row is, from 0 at the top to 1 at the bottom.
    pub start: f32,
    /// Where its rows end.
    pub end: f32,
    /// Its tone's solid color.
    pub color: Hsla,
}

/// The hunks marked on the diff's scrollbar track: each one that still needs the reviewer, and
/// each flagged one, in its tone's color, so what is left of a long diff shows without
/// scrolling to it.
#[must_use]
pub fn ruler(model: &Model, palette: &Palette) -> Vec<RulerMark> {
    let rows = crate::scroll::float(model.rows.screen.len());
    if rows == 0.0 {
        return Vec::new();
    }
    model
        .hunk_rows
        .iter()
        .enumerate()
        .filter_map(|(hunk, range)| {
            let tone = hunk_tone(model, hunk);
            (tone != HunkTone::Approved).then(|| RulerMark {
                start: crate::scroll::float(range.start) / rows,
                end: crate::scroll::float(range.end) / rows,
                color: palette.hunk_solid(tone),
            })
        })
        .collect()
}

/// Where a line of code is drawn: its screen row, its file section and its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    /// The screen row, by index into [`crate::rows::Layout::screen`].
    pub row: usize,
    /// The file section, by index into the layout's files.
    pub file: usize,
    /// The column: the whole width, or a half of a side-by-side row.
    pub column: Column,
}

/// A line of code as the diff draws it: its gutter, its sign and its text's runs on the line's
/// colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeCell {
    /// The line's number in its file, `┆` for a nested line across the width, or empty.
    pub number: Piece,
    /// The change bar along the gutter's edge, in a style with one.
    pub bar: Option<Hsla>,
    /// `−`, `+` or a blank.
    pub sign: Piece,
    /// The runs over the text, bottom first: syntax colors, find matches, the selection and
    /// emphasized tokens; [`merge`](super::merge) flattens them.
    pub layers: Vec<Layer>,
    /// The line's background, for a changed line in a style with line backgrounds.
    pub background: Option<Hsla>,
    /// The text color, for a changed line in a style without syntax colors.
    pub text_color: Option<Hsla>,
    /// The quiet `⏎` that stands for a blank changed line, so it never reads as an empty bar.
    pub blank: Option<Piece>,
    /// The bytes of the line's text the screen row shows, which [`Self::layers`] cover from
    /// their start: all of it, a piece of a wrapped line, or nothing on a side whose line ended
    /// on an earlier screen row.
    pub shown: Range<usize>,
}

/// How the diff style inks a line of code.
struct CellInk {
    /// `−`, `+` or blank.
    sign: &'static str,
    sign_color: Hsla,
    /// The line's background, when the style draws one.
    background: Option<Hsla>,
    /// The text color, for a changed line in a style without syntax colors.
    text_color: Option<Hsla>,
    /// The change bar's color, for a changed line in a style with one.
    bar: Option<Hsla>,
}

/// Line of code `cell` as the diff draws it at `at`: the piece of it the screen row shows,
/// with the line number and sign on its first piece only and a dim `↪` on each piece after.
#[must_use]
pub fn code_cell(model: &Model, palette: &Palette, at: At, cell: &Cell) -> CodeCell {
    let piece = model.rows.screen.get(at.row);
    // Selections and find matches name the row, whichever of its screen rows draws.
    let line = piece.map_or(at.row, |piece| piece.row);
    let side = usize::from(at.column == Column::Right);
    let shown = match piece {
        Some(piece) => piece.shown.get(side).cloned().flatten(),
        None => Some(0..cell.text.len()),
    };
    let first = piece.is_none() || model.rows.starts_row(at.row);
    let mut ink = cell_ink(model, cell, palette);
    let frame = frame(model, at.row);
    // Bold backgrounds only on the selected hunk, when asked, so it stands out.
    let look = model.look();
    if look.soft_unselected && look.style.background == Background::Bold && !frame.selected() {
        ink.background = ink
            .background
            .map(|line| theme::mix(line, palette.background, crate::look::MUTED_SHARE));
    }
    // Approved code loses its diff colors in every style, its signs aside, so the code still to
    // review is what stands out: added lines read as the file's code, and removed lines fade,
    // as code that is gone.
    let approved =
        hunk_at(model, at.row).is_some_and(|hunk| hunk_tone(model, hunk) == HunkTone::Approved);
    let faded = approved && matches!(cell.kind, RowKind::Removed | RowKind::NestedOld);
    if approved {
        ink.background = None;
        ink.text_color = faded.then_some(palette.faint);
        ink.bar = None;
    }
    // Only the runs within what the screen row shows are built, so a long line that wraps
    // onto many screen rows is not styled whole for each of them.
    let bounds = shown.clone().unwrap_or(cell.text.len()..cell.text.len());
    let syntax = if faded {
        Vec::new()
    } else {
        syntax_layer(model, palette, at.file, cell, &bounds)
    };
    let mut layers = vec![syntax];
    if !approved {
        layers.push(moved_edit_layer(cell, palette, &bounds));
    }
    // Under the find matches and the selection, which tint behind the text and leave deleted
    // text its red and its line.
    layers.push(marks_layer(model, cell, palette, approved, &bounds));
    if let Some(find) = model.search_of(Mode::Diff) {
        let current = find.current.and_then(|current| find.matches.get(current));
        layers.push(around_deleted(
            match_layer(&find.matches, current, line, at.column, &bounds, palette),
            &cell.deleted,
        ));
    }
    if let Some(range) = model
        .selection
        .filter(|selection| selection.pane == Pane::Diff && selection.covers(at.column))
        .and_then(|selection| selection.range_in_row(line, cell.content_len))
    {
        layers.push(around_deleted(
            vec![Run {
                range,
                style: Style::behind(palette.selection),
            }],
            &cell.deleted,
        ));
    }
    let blank = (first
        && cell.text.is_empty()
        && matches!(
            cell.kind,
            RowKind::Removed | RowKind::Added | RowKind::Edited
        ))
    .then(|| Piece::new("⏎", palette.faint));
    let (number, sign) = match &shown {
        _ if first => (
            gutter(cell, at.column, frame.selected(), palette),
            Piece::new(ink.sign, ink.sign_color),
        ),
        Some(_) => (
            Piece::new("", palette.faint),
            Piece::new("↪", palette.faint),
        ),
        None => {
            // A side whose line ended shows its background alone, without the change bar.
            ink.bar = None;
            (
                Piece::new("", palette.faint),
                Piece::new(" ", palette.faint),
            )
        }
    };
    let shown = shown.unwrap_or(cell.text.len()..cell.text.len());
    CodeCell {
        number,
        bar: ink.bar,
        sign,
        layers: layers
            .into_iter()
            .map(|layer| within(layer, &shown))
            .collect(),
        background: ink.background,
        text_color: ink.text_color,
        blank,
        shown,
    }
}

/// The runs of `layer` within the bytes `shown`, moved to start where `shown` starts.
fn within(layer: Layer, shown: &Range<usize>) -> Layer {
    layer
        .into_iter()
        .filter_map(|run| {
            let start = run.range.start.max(shown.start);
            let end = run.range.end.min(shown.end);
            (start < end).then(|| Run {
                range: start - shown.start..end - shown.start,
                style: run.style,
            })
        })
        .collect()
}

/// Whether `cell` is a line of a nested diff: an interdiff's or a move's edits.
fn is_nested(cell: &Cell) -> bool {
    matches!(
        cell.kind,
        RowKind::NestedOld | RowKind::NestedNew | RowKind::NestedSame
    )
}

/// A line's number in its file, painted in `column`: in the text color in the selected hunk, so
/// the hunk the keys act on shows in the gutter too, and quiet elsewhere.
fn gutter(cell: &Cell, column: Column, selected: bool, palette: &Palette) -> Piece {
    let nested = is_nested(cell);
    // Each half of a pair numbers its own side: the left the base (or a nested diff's old
    // side), the right the head.
    // Across the width, a nested line has no single number, except a line of moved code where
    // it arrived, which is a line of this file: an old line of a move's edits has none.
    let line = match column {
        Column::Left => cell.base,
        Column::Right => cell.head.as_ref().map(|head| head.line),
        Column::Whole if nested && cell.moved && cell.kind != RowKind::NestedOld => {
            cell.head.as_ref().map(|head| head.line)
        }
        Column::Whole if nested => None,
        Column::Whole => cell.head.as_ref().map(|head| head.line).or(cell.base),
    };
    let number = match line {
        Some(line) => format!("{}", line + 1),
        None if nested => "┆".to_owned(),
        None => String::new(),
    };
    let color = if selected {
        palette.text
    } else {
        palette.faint
    };
    let color = if cell.kind == RowKind::Removed {
        color.alpha(0.6)
    } else {
        color
    };
    Piece::new(number, color)
}

/// How the diff style inks a line of code: its sign, background, text color and change bar.
///
/// A formatting-only line is shown, but never as a removal or an addition.
fn cell_ink(model: &Model, cell: &Cell, palette: &Palette) -> CellInk {
    let style = &model.look().style;
    let line = |color: Hsla| (style.background != Background::None).then_some(color);
    let text = |color: Hsla| (!style.syntax).then_some(color);
    let bar = |color: Hsla| style.change_bar.then_some(color);
    match cell.kind {
        RowKind::Removed | RowKind::NestedOld | RowKind::Added | RowKind::NestedNew
            if cell.quiet =>
        {
            CellInk {
                sign: if matches!(cell.kind, RowKind::Removed | RowKind::NestedOld) {
                    "−"
                } else {
                    "+"
                },
                sign_color: palette.faint,
                background: line(palette.quiet_line),
                text_color: None,
                bar: bar(palette.faint),
            }
        }
        // Moved code, and the lines a move with edits keeps, in the move's color; the edits
        // themselves stay removed and added.
        RowKind::Removed | RowKind::Added | RowKind::NestedSame if cell.moved => CellInk {
            sign: match cell.kind {
                RowKind::Removed => "−",
                RowKind::Added => "+",
                _ => " ",
            },
            sign_color: palette.moved,
            background: line(palette.moved_line),
            text_color: text(palette.moved),
            bar: bar(palette.moved),
        },
        RowKind::Removed | RowKind::NestedOld => CellInk {
            sign: "−",
            sign_color: palette.removed_sign,
            background: line(palette.removed_line),
            text_color: text(palette.removed_text),
            bar: bar(palette.removed_sign),
        },
        RowKind::Added | RowKind::NestedNew => CellInk {
            sign: "+",
            sign_color: palette.added_sign,
            background: line(palette.added_line),
            text_color: text(palette.added_text),
            bar: bar(palette.added_sign),
        },
        // A line changed in place reads as the code it now is, what stayed the same as any
        // unchanged line, its marks saying what changed, on a soft background of its own while
        // it still needs the reviewer.
        RowKind::Edited => CellInk {
            sign: "~",
            sign_color: palette.added_sign,
            background: line(palette.edited_line),
            text_color: None,
            bar: bar(palette.added_sign),
        },
        _ => CellInk {
            sign: " ",
            sign_color: palette.faint,
            background: None,
            text_color: None,
            bar: None,
        },
    }
}

/// The marker over the last cell of a line of code that runs past the right edge of its room,
/// so a clipped line never reads as complete; it covers the code under it, on the line's own
/// background.
#[must_use]
pub fn clip_marker(palette: &Palette) -> Piece {
    Piece::new("…", palette.dim)
}

/// Whether a line of code `cells` wide, its code moved `offset` cells left, runs past the right
/// edge of `room` cells, where [`clip_marker`] shows.
#[must_use]
pub fn clipped(cells: usize, offset: f64, room: f64) -> bool {
    crate::scroll::rows(cells) - offset > room
}

/// Syntax colors for the runs of one line that lie within its first `len` bytes; headings and
/// strong markup are bold and emphasized markup italic, as well as colored.
#[must_use]
pub fn syntax_runs(theme: &Theme, spans: &[Span], len: usize) -> Layer {
    spans
        .iter()
        .filter(|(range, _)| range.end <= len)
        .filter_map(|(range, tone)| {
            Some(Run {
                range: range.clone(),
                style: Style {
                    bold: matches!(tone, Tone::Heading | Tone::Strong),
                    italic: *tone == Tone::Emphasis,
                    ..Style::text(theme.syntax(*tone)?)
                },
            })
        })
        .collect()
}

/// The syntax colors of a line of code, in a style with syntax colors.
///
/// A style without them, as lazygit draws a diff, has none anywhere, only red and green
/// changed lines.
/// A line of the head takes the head's colors, any other line of the base the base's.
fn syntax_layer(
    model: &Model,
    palette: &Palette,
    file: usize,
    cell: &Cell,
    bounds: &Range<usize>,
) -> Layer {
    let style = &model.look().style;
    if !style.syntax {
        return Vec::new();
    }
    let Some(syntax) = model
        .rows
        .files
        .get(file)
        .and_then(|section| model.folds.syntax.get(&section.path))
    else {
        return Vec::new();
    };
    // A nested line's old side comes from another file or version than this file's base,
    // whose colors it never takes.
    let spans = match (&cell.head, cell.base) {
        (Some(head), _) => syntax.head.as_ref().map(|side| side.line(head.line)),
        (None, Some(base)) if !is_nested(cell) => syntax.base.as_ref().map(|side| side.line(base)),
        (None, _) => None,
    };
    // An edited line's own bytes follow the deleted text spliced into it; only the spans that
    // reach into what the screen row shows are moved there.
    let shift = |(range, tone): &Span| {
        let start = cell.shown_at(range.start, true);
        (start..cell.shown_at(range.end, false), *tone)
    };
    let spans: Vec<Span> = overlapping(spans.unwrap_or_default(), bounds, |span| shift(span).0)
        .iter()
        .map(shift)
        .collect();
    let mut layer = syntax_runs(model.theme(), &spans, cell.content_len);
    // On a tinted line, dim colors such as comments lift toward the text color until they
    // stay readable against the line's final background.
    let tint = match cell.kind {
        _ if style.background == Background::None => None,
        _ if cell.quiet => Some(palette.quiet_line),
        RowKind::Removed | RowKind::Added | RowKind::NestedSame if cell.moved => {
            Some(palette.moved_line)
        }
        RowKind::Removed | RowKind::NestedOld => Some(palette.removed_line),
        RowKind::Added | RowKind::NestedNew => Some(palette.added_line),
        _ => None,
    };
    if let Some(tint) = tint {
        for run in &mut layer {
            run.style.color = run
                .style
                .color
                .map(|color| theme::legible(color, tint, palette.text));
        }
    }
    layer
}

/// The words a move's edit changed, behind a stronger shade of the line's color, as delta marks
/// them, so what the move edited stands out within the code it carried; and the indentation a
/// re-indented line gained, in the move's color.
fn moved_edit_layer(cell: &Cell, palette: &Palette, bounds: &Range<usize>) -> Layer {
    let color = match cell.kind {
        _ if !cell.moved => return Vec::new(),
        RowKind::NestedOld => palette.removed_sign,
        RowKind::NestedNew => palette.added_sign,
        // The indentation a re-indented line gained.
        RowKind::NestedSame => palette.moved,
        _ => return Vec::new(),
    };
    within_bounds(&cell.highlights, bounds)
        .map(|range| Run {
            range: range.clone(),
            style: Style::behind(color.alpha(MOVED_EDIT_ALPHA)),
        })
        .collect()
}

/// How strongly the words a move's edit changed show behind their text.
const MOVED_EDIT_ALPHA: f32 = 0.4;

/// How strongly a changed word shows behind its text.
const MARK_ALPHA: f32 = 0.4;

/// The words a line changed, behind a stronger shade of red or green, as delta marks them. An
/// edited line always marks the tokens it inserted and strikes out the ones it deleted, which
/// never read as code, in red until approved and gray after; a removed or an added line marks
/// its changed words when the look emphasizes them.
fn marks_layer(
    model: &Model,
    cell: &Cell,
    palette: &Palette,
    approved: bool,
    bounds: &Range<usize>,
) -> Layer {
    if cell.kind == RowKind::Edited {
        // Deleted text keeps a tint when approved too: it is no code, and a deleted space, or
        // any deleted text in a terminal that draws no line through it, has no other cue.
        let color = if approved {
            palette.faint
        } else {
            palette.removed_sign
        };
        let deleted = Style {
            color: Some(color),
            background: Some(color.alpha(MARK_ALPHA)),
            strikethrough: true,
            ..Style::default()
        };
        let mut layer: Layer = within_bounds(&cell.deleted, bounds)
            .map(|range| Run {
                range: range.clone(),
                style: deleted,
            })
            .collect();
        if !approved {
            layer.extend(within_bounds(&cell.highlights, bounds).map(|range| Run {
                range: range.clone(),
                style: Style::behind(palette.added_sign.alpha(MARK_ALPHA)),
            }));
        }
        layer.sort_by_key(|run| run.range.start);
        return layer;
    }
    let color = match cell.kind {
        _ if approved || cell.quiet || cell.moved || !model.look().token_emphasis => {
            return Vec::new();
        }
        RowKind::Removed => palette.removed_sign,
        RowKind::Added => palette.added_sign,
        _ => return Vec::new(),
    };
    within_bounds(&cell.highlights, bounds)
        .map(|range| Run {
            range: range.clone(),
            style: Style::behind(color.alpha(MARK_ALPHA)),
        })
        .collect()
}

/// The ranges of `ranges` that reach into `bounds`.
fn within_bounds<'a>(
    ranges: &'a [Range<usize>],
    bounds: &Range<usize>,
) -> impl Iterator<Item = &'a Range<usize>> {
    let bounds = bounds.clone();
    ranges
        .iter()
        .filter(move |range| range.start < bounds.end && bounds.start < range.end)
}

/// The items of `items` that reach into `bounds`, found by binary search: `range` gives each
/// item's bytes, which are sorted and do not overlap.
fn overlapping<'a, T>(
    items: &'a [T],
    bounds: &Range<usize>,
    range: impl Fn(&T) -> Range<usize>,
) -> &'a [T] {
    let start = items.partition_point(|item| range(item).end <= bounds.start);
    let rest = items.get(start..).unwrap_or_default();
    let end = rest.partition_point(|item| range(item).start < bounds.end);
    rest.get(..end).unwrap_or_default()
}

/// `layer` apart from an edited line's `deleted` text, which is no code to find or select, so
/// inside a match or a selection it keeps its own red and line.
fn around_deleted(layer: Layer, deleted: &[std::ops::Range<usize>]) -> Layer {
    if deleted.is_empty() {
        return layer;
    }
    let mut parts = Vec::new();
    for run in layer {
        let mut start = run.range.start;
        for range in deleted
            .iter()
            .filter(|range| range.start < run.range.end && run.range.start < range.end)
        {
            if start < range.start {
                parts.push(Run {
                    range: start..range.start,
                    style: run.style,
                });
            }
            start = start.max(range.end);
        }
        if start < run.range.end {
            parts.push(Run {
                range: start..run.range.end,
                style: run.style,
            });
        }
    }
    parts
}

/// The find matches on row `row` in `column` that reach into `bounds`: the current one as a
/// solid block, the others tinted.
///
/// Matches are sorted by row, then column, then bytes, so those are found by binary search.
fn match_layer(
    matches: &[Match],
    current: Option<&Match>,
    row: usize,
    column: Column,
    bounds: &Range<usize>,
    palette: &Palette,
) -> Layer {
    let start = matches.partition_point(|found| (found.row, found.column) < (row, column));
    let ours = matches.get(start..).unwrap_or_default();
    let ours = ours
        .get(..ours.partition_point(|found| (found.row, found.column) == (row, column)))
        .unwrap_or_default();
    overlapping(ours, bounds, |found| found.range.clone())
        .iter()
        .map(|found| Run {
            range: found.range.clone(),
            style: if current == Some(found) {
                Style::block(palette.accent, palette.background)
            } else {
                Style::behind(palette.find)
            },
        })
        .collect()
}

/// What the buffer's lines need to draw, apart from the model, so a front end can build lines
/// lazily as its list asks for them.
#[derive(Clone)]
pub struct BufferLines {
    palette: Palette,
    theme: Arc<Theme>,
    lines: Arc<FullText>,
    syntax: Option<Arc<SideSyntax>>,
    markers: Arc<Vec<Option<Marker>>>,
    matches: Arc<Vec<Match>>,
    current: Option<Match>,
    selection: Option<Selection>,
    cursor: TextPos,
}

/// A line of the buffer: its gutter, its runs and its background.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferLine {
    /// The line's number, brighter on the cursor's line.
    pub number: Piece,
    /// What the line changed, for the marker in its gutter: an added line's bar in
    /// [`Palette::added_sign`], a rule above the line where lines were removed in
    /// [`Palette::removed_sign`].
    pub marker: Option<Marker>,
    /// The runs over the text, bottom first: syntax colors, find matches, the selection and
    /// the cursor.
    pub layers: Vec<Layer>,
    /// The cursor line's background.
    pub background: Option<Hsla>,
}

impl BufferLines {
    /// The open buffer's lines in `palette`, with the buffer's find and selection.
    #[must_use]
    pub fn of(model: &Model, palette: Palette) -> Option<Self> {
        let buffer = model.buffer.as_ref()?;
        let find = model.search_of(Mode::Buffer);
        let matches: Arc<Vec<Match>> = find
            .map(|find| Arc::clone(&find.matches))
            .unwrap_or_default();
        let current = find
            .and_then(|find| find.current)
            .and_then(|index| matches.get(index).cloned());
        Some(Self {
            palette,
            theme: Arc::clone(&model.look().theme),
            lines: Arc::clone(&buffer.lines),
            syntax: buffer.syntax.clone(),
            markers: Arc::clone(&buffer.markers),
            matches,
            current,
            selection: model
                .selection
                .filter(|selection| selection.pane == Pane::Source),
            cursor: buffer.cursor,
        })
    }

    /// The buffer's text.
    #[must_use]
    pub fn text(&self) -> &Arc<FullText> {
        &self.lines
    }

    /// Line `index`, when the buffer has one.
    #[must_use]
    pub fn line(&self, index: usize) -> Option<BufferLine> {
        let line = self.lines.lines.get(index)?;
        let palette = &self.palette;
        let mut layers = Vec::new();
        if let Some(syntax) = &self.syntax {
            layers.push(syntax_runs(&self.theme, syntax.line(index), line.len()));
        }
        layers.push(match_layer(
            &self.matches,
            self.current.as_ref(),
            index,
            Column::Whole,
            &(0..line.len()),
            palette,
        ));
        if let Some(range) = self
            .selection
            .and_then(|selection| selection.range_in_row(index, line.len()))
        {
            layers.push(vec![Run {
                range,
                style: Style::behind(palette.selection),
            }]);
        }
        let on_cursor = index == self.cursor.row;
        if on_cursor {
            let col = self.cursor.col;
            let end = line
                .get(col..)
                .and_then(|rest| rest.chars().next())
                .map_or(col, |next| col + next.len_utf8());
            if end > col {
                layers.push(vec![Run {
                    range: col..end,
                    style: Style::block(palette.text, palette.background),
                }]);
            }
        }
        Some(BufferLine {
            number: Piece::new(
                format!("{}", index + 1),
                if on_cursor {
                    palette.text
                } else {
                    palette.faint
                },
            ),
            marker: self.markers.get(index).copied().flatten(),
            layers,
            background: on_cursor.then_some(palette.raised),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A find match or a selection across an edited line's deleted text paints only the code
    /// around it, which leaves the deleted text its own red and line.
    #[test]
    fn find_and_selection_skip_deleted_text() {
        let style = Style::default();
        let runs = |ranges: &[(usize, usize)]| -> Layer {
            ranges
                .iter()
                .map(|&(start, end)| Run {
                    range: start..end,
                    style,
                })
                .collect()
        };
        // `min(3220)`: `32` deleted, `20` inserted.
        let deleted: Vec<std::ops::Range<usize>> = std::iter::once(4..6).collect();
        assert_eq!(
            around_deleted(runs(&[(0, 9)]), &deleted),
            runs(&[(0, 4), (6, 9)])
        );
        assert_eq!(around_deleted(runs(&[(4, 6)]), &deleted), runs(&[]));
        assert_eq!(around_deleted(runs(&[(0, 9)]), &[]), runs(&[(0, 9)]));
    }
}
