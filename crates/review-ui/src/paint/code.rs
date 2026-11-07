//! Lines of code: their styled runs (syntax colors, emphasized tokens, find matches, the
//! selection and the cursor), how the diff style inks them, their gutters, and the bar along a
//! hunk's rows.

use std::sync::Arc;

use super::palette::HunkTone;
use super::{Layer, Palette, Piece, Run, Style};
use crate::color::Hsla;
use crate::config::Background;
use crate::find::Match;
use crate::model::buffer::Marker;
use crate::model::{Mode, Model};
use crate::rows::{Body, Cell, FullText, Row, RowKind};
use crate::syntax::{SideSyntax, Span};
use crate::text::{Column, Pane, Selection, TextPos};
use crate::theme::{self, Theme};

/// Where a diff row is relative to the hunks, for its band and the bar along its left edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    /// Between hunks: no band and no bar.
    Outside,
    /// In a hunk that is not selected: a gray bar on the hunk's gray band, so every hunk's
    /// extent shows.
    Hunk,
    /// In the selected hunk, of its tone: a bar in the tone's solid color.
    Selected(HunkTone),
}

impl Frame {
    /// The bar along the row's left edge: gray, or the selected hunk's solid color.
    #[must_use]
    pub fn bar(self, palette: &Palette) -> Option<Hsla> {
        match self {
            Self::Outside => None,
            Self::Hunk => Some(palette.faint),
            Self::Selected(tone) => Some(palette.hunk_solid(tone)),
        }
    }

    /// The band behind a hunk's rows; a changed line's own background covers it.
    #[must_use]
    pub fn band(self, palette: &Palette) -> Option<Hsla> {
        (self != Self::Outside).then_some(palette.band)
    }
}

/// Where diff row `index` is relative to the hunks: in the selected one only while the diff
/// has the keys.
#[must_use]
pub fn frame(model: &Model, index: usize) -> Frame {
    let selected = (model.mode == Mode::Diff).then_some(model.hunk);
    match hunk_at(model, index) {
        Some(hunk) if Some(hunk) == selected => Frame::Selected(hunk_tone(model, hunk)),
        Some(_) => Frame::Hunk,
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

/// The hunk that diff row `index` belongs to for its band and bar: from the hunk's header
/// through its last row and the unchanged lines right after it, up to the next gap or header.
fn hunk_at(model: &Model, index: usize) -> Option<usize> {
    // Hunks are in row order, so the one that may hold the row is the last starting at or
    // before it.
    let hunk = model
        .hunk_rows
        .partition_point(|rows| rows.start <= index)
        .checked_sub(1)?;
    let rows = model.hunk_rows.get(hunk)?;
    let trailing = |row: &Row| row.hunk.is_none() && !matches!(row.body, Body::Note(_));
    let within = rows.contains(&index)
        || (index >= rows.end
            && model
                .rows
                .rows
                .get(rows.end..=index)
                .is_some_and(|after| after.iter().all(trailing)));
    within.then_some(hunk)
}

/// Where a line of code is drawn: its diff row, its file section and its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    /// The diff row.
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

/// Line of code `cell` as the diff draws it at `at`.
#[must_use]
pub fn code_cell(model: &Model, palette: &Palette, at: At, cell: &Cell) -> CodeCell {
    let ink = cell_ink(model, cell, palette);
    let mut layers = vec![syntax_layer(model, palette, at.file, cell)];
    if let Some(find) = model.search_of(Mode::Diff) {
        let current = find.current.and_then(|current| find.matches.get(current));
        layers.push(match_layer(
            &find.matches,
            current,
            at.row,
            Some(at.column),
            palette,
        ));
    }
    if let Some(range) = model
        .selection
        .filter(|selection| selection.pane == Pane::Diff && selection.covers(at.column))
        .and_then(|selection| selection.range_in_row(at.row, cell.content_len))
    {
        layers.push(vec![Run {
            range,
            style: Style::behind(palette.selection),
        }]);
    }
    layers.push(emphasis_layer(model, cell, palette));
    let blank = (cell.text.is_empty() && matches!(cell.kind, RowKind::Removed | RowKind::Added))
        .then(|| Piece::new("⏎", palette.faint));
    CodeCell {
        number: gutter(cell, at.column, palette),
        bar: ink.bar,
        sign: Piece::new(ink.sign, ink.sign_color),
        layers,
        background: ink.background,
        text_color: ink.text_color,
        blank,
    }
}

/// Whether `cell` is a line of a nested diff: an interdiff's or a move's edits.
fn is_nested(cell: &Cell) -> bool {
    matches!(
        cell.kind,
        RowKind::NestedOld | RowKind::NestedNew | RowKind::NestedSame
    )
}

/// A line's number in its file, painted in `column`.
fn gutter(cell: &Cell, column: Column, palette: &Palette) -> Piece {
    let nested = is_nested(cell);
    // Each half of a pair numbers its own side: the left the base (or a nested diff's old
    // side), the right the head.
    // Across the width, a nested line has no single number.
    let line = match column {
        Column::Left => cell.base,
        Column::Right => cell.head.as_ref().map(|head| head.line),
        Column::Whole if nested => None,
        Column::Whole => cell.head.as_ref().map(|head| head.line).or(cell.base),
    };
    let number = match line {
        Some(line) => format!("{}", line + 1),
        None if nested => "┆".to_owned(),
        None => String::new(),
    };
    let color = if cell.kind == RowKind::Removed {
        palette.faint.alpha(0.6)
    } else {
        palette.faint
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

/// Syntax colors for the runs of one line that lie within its first `len` bytes.
#[must_use]
pub fn syntax_runs(theme: &Theme, spans: &[Span], len: usize) -> Layer {
    spans
        .iter()
        .filter(|(range, _)| range.end <= len)
        .filter_map(|(range, tone)| {
            Some(Run {
                range: range.clone(),
                style: Style::text(theme.syntax(*tone)?),
            })
        })
        .collect()
}

/// The syntax colors of a line of code, in a style with syntax colors.
///
/// A style without them, as lazygit draws a diff, has none anywhere, only red and green
/// changed lines.
/// A line of the head takes the head's colors, any other line of the base the base's.
fn syntax_layer(model: &Model, palette: &Palette, file: usize, cell: &Cell) -> Layer {
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
    let mut layer = syntax_runs(model.theme(), spans.unwrap_or_default(), cell.content_len);
    // On a tinted line, dim colors such as comments lift toward the text color until they
    // stay readable against the line's final background.
    let tint = match cell.kind {
        _ if style.background == Background::None => None,
        _ if cell.quiet => Some(palette.quiet_line),
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

/// The underlines of the tokens a line really changed, when the look emphasizes them.
fn emphasis_layer(model: &Model, cell: &Cell, palette: &Palette) -> Layer {
    if !model.look().token_emphasis || cell.quiet {
        return Vec::new();
    }
    let underline = Style {
        underline: Some(palette.text.alpha(0.5)),
        ..Style::default()
    };
    cell.highlights
        .iter()
        .map(|range| Run {
            range: range.clone(),
            style: underline,
        })
        .collect()
}

/// The find matches on row `row`, in `column` when one is given: the current one as a solid
/// block, the others tinted.
fn match_layer(
    matches: &[Match],
    current: Option<&Match>,
    row: usize,
    column: Option<Column>,
    palette: &Palette,
) -> Layer {
    let start = matches.partition_point(|found| found.row < row);
    matches
        .get(start..)
        .unwrap_or_default()
        .iter()
        .take_while(|found| found.row == row)
        .filter(|found| column.is_none_or(|column| found.column == column))
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
            None,
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
