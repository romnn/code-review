//! Drawing the window: the files, the diff or buffer, the bars and the overlays.
//!
//! Code and our own annotations never share a look. Code rows sit on the editor background with
//! a stable line-number gutter; everything the tool adds (hunk headers, gaps, notes, moves) sits
//! on a distinct band in muted colors, without a gutter.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    AnyElement, Context, Decorations, Div, FontWeight, HighlightStyle, Hsla,
    ListHorizontalSizingBehavior, MouseButton, ScrollWheelEvent, SharedString, Stateful,
    StyledText, UnderlineStyle, Window, div, px, relative, uniform_list,
};
use gpui_component::input::Input;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{h_flex, v_flex, window_border};
use review_core::tree::NodeKind;
use review_core::{FileTone, UnitId};

use super::buffer::Marker;
use super::keymap::Area;
use super::present::FRAME;
use super::state::{self, Progress};
use super::text::{Column, Pane};
use super::{Mode, Overlay, Phase, ReviewApp, scroll};
use crate::config::{Background, Headers};
use crate::look::{self, Look};
use crate::rows::{Badge, Body, Cell, Note, Row, RowKind};
use crate::syntax::Span;
use crate::theme::Theme;

/// How long an error tint lasts.
pub const FLASH: Duration = Duration::from_millis(450);

/// The grid every spacing is a multiple of.
pub const GRID: f32 = 4.0;

/// The height of the pane headers and the bars.
pub const BAR: f32 = GRID * 8.0;

/// The colors of the window, by role, from the theme and the configured diff colors.
#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Hsla,
    pub surface: Hsla,
    pub band: Hsla,
    pub raised: Hsla,
    pub hairline: Hsla,
    pub text: Hsla,
    pub dim: Hsla,
    pub faint: Hsla,
    pub accent: Hsla,
    pub selection: Hsla,
    pub find: Hsla,
    pub error: Hsla,
    pub open: Hsla,
    pub accepted: Hsla,
    pub flagged: Hsla,
    pub changed: Hsla,
    pub moved: Hsla,
    pub collapsed: Hsla,
    pub kind_added: Hsla,
    pub kind_modified: Hsla,
    pub kind_deleted: Hsla,
    pub kind_renamed: Hsla,
    pub kind_type: Hsla,
    pub kind_quiet: Hsla,
    pub removed_line: Hsla,
    pub added_line: Hsla,
    pub quiet_line: Hsla,
    pub removed_text: Hsla,
    pub added_text: Hsla,
    pub removed_sign: Hsla,
    pub added_sign: Hsla,
}

impl Palette {
    /// The palette of `theme`, with the configured line backgrounds when there are any.
    ///
    /// Muted line backgrounds are mixed toward the editor background here.
    /// Everything that tints a line reads these final backgrounds, so lifting syntax colors to
    /// stay legible measures against them.
    pub fn new(
        theme: &Theme,
        removed: Option<u32>,
        added: Option<u32>,
        background: Background,
    ) -> Self {
        let line = |hex: Option<u32>, fallback: Hsla| {
            let color = hex.map_or(fallback, |hex| gpui::rgb(hex).into());
            match background {
                Background::None | Background::Bold => color,
                Background::Muted => {
                    crate::theme::mix(color, theme.ui.background, look::MUTED_SHARE)
                }
            }
        };
        Self {
            background: theme.ui.background,
            surface: theme.ui.panel,
            band: theme.ui.band,
            raised: theme.ui.raised,
            hairline: theme.ui.hairline,
            text: theme.ui.text,
            dim: theme.ui.dim,
            faint: theme.ui.faint,
            accent: theme.ui.accent,
            selection: Hsla {
                a: 0.6,
                ..theme.ui.selection
            },
            find: Hsla {
                a: 0.45,
                ..theme.ui.find
            },
            error: theme.ui.error,
            open: theme.review.open,
            accepted: theme.review.done,
            flagged: theme.review.flagged,
            changed: theme.review.changed,
            moved: theme.kind.moved,
            collapsed: theme.kind.quiet,
            kind_added: theme.kind.added,
            kind_modified: theme.kind.modified,
            kind_deleted: theme.kind.deleted,
            kind_renamed: theme.kind.renamed,
            kind_type: theme.kind.type_change,
            kind_quiet: theme.kind.quiet,
            removed_line: line(removed, theme.diff.removed_line),
            added_line: line(added, theme.diff.added_line),
            quiet_line: theme.diff.quiet_line,
            removed_text: theme.diff.removed_text,
            added_text: theme.diff.added_text,
            removed_sign: theme.diff.removed_sign,
            added_sign: theme.diff.added_sign,
        }
    }

    /// The color of a file's change tone.
    pub fn tone(&self, tone: FileTone) -> Hsla {
        match tone {
            FileTone::Added => self.kind_added,
            FileTone::Modified => self.kind_modified,
            FileTone::Deleted => self.kind_deleted,
            FileTone::Renamed => self.kind_renamed,
            FileTone::Moved => self.moved,
            FileTone::TypeChanged => self.kind_type,
            FileTone::FormattingOnly | FileTone::Generated => self.kind_quiet,
        }
    }

    /// The color of a review progress.
    pub fn progress(&self, progress: Progress) -> Hsla {
        match progress {
            Progress::Open | Progress::Partial => self.open,
            Progress::Done => self.accepted,
            Progress::Flagged => self.flagged,
            Progress::Changed => self.changed,
        }
    }

    /// The color of a hunk badge.
    pub fn badge(&self, badge: Badge) -> Hsla {
        match badge {
            Badge::Unreviewed => self.open,
            Badge::Seen | Badge::Accepted => self.accepted,
            Badge::Flagged => self.flagged,
            Badge::Changed => self.changed,
        }
    }
}

/// The glyph of a review progress: one per file or folder.
pub(super) fn progress_glyph(progress: Progress) -> &'static str {
    match progress {
        // Only state that needs action or confirms done carries ink.
        Progress::Open => "",
        Progress::Partial => "◐",
        Progress::Done => "✓",
        Progress::Flagged => "!",
        Progress::Changed => "↻",
    }
}

/// The glyph of a hunk's state.
fn badges_glyph(badges: &[Badge]) -> (Badge, &'static str) {
    let all = |wanted: fn(Badge) -> bool| !badges.is_empty() && badges.iter().copied().all(wanted);
    if badges.contains(&Badge::Flagged) {
        (Badge::Flagged, "!")
    } else if badges.contains(&Badge::Changed) {
        (Badge::Changed, "↻")
    } else if all(state::is_approved) {
        (Badge::Accepted, "✓")
    } else if badges.iter().copied().any(state::is_approved) {
        (Badge::Unreviewed, "◐")
    } else {
        (Badge::Unreviewed, "○")
    }
}

/// Flattens layered highlights into sorted, non-overlapping runs; later layers win where they
/// set a field. Each layer's runs are sorted and do not overlap, so one sweep over the cut
/// points, with one cursor per layer, merges them in linear time.
pub fn merge_highlights(
    len: usize,
    layers: &[Vec<(Range<usize>, HighlightStyle)>],
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut cuts: Vec<usize> = layers
        .iter()
        .flatten()
        .flat_map(|(range, _)| [range.start.min(len), range.end.min(len)])
        .collect();
    if cuts.is_empty() {
        return Vec::new();
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut cursors = vec![0_usize; layers.len()];
    let mut merged: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for pair in cuts.windows(2) {
        let &[start, end] = pair else {
            continue;
        };
        let mut style: Option<HighlightStyle> = None;
        for (layer, cursor) in layers.iter().zip(cursors.iter_mut()) {
            while layer
                .get(*cursor)
                .is_some_and(|(range, _)| range.end <= start)
            {
                *cursor += 1;
            }
            if let Some((range, layered)) = layer.get(*cursor)
                && range.start <= start
                && end <= range.end
            {
                style = Some(style.map_or(*layered, |style| style.highlight(*layered)));
            }
        }
        if let Some(style) = style {
            match merged.last_mut() {
                Some((range, last)) if range.end == start && *last == style => range.end = end,
                _ => merged.push((start..end, style)),
            }
        }
    }
    merged
}

fn background(color: Hsla) -> HighlightStyle {
    HighlightStyle {
        background_color: Some(color),
        ..HighlightStyle::default()
    }
}

/// A solid block, such as the cursor or the current match: `color` behind text in `text`, so
/// the text stays readable whatever its syntax color.
fn block(color: Hsla, text: Hsla) -> HighlightStyle {
    HighlightStyle {
        color: Some(text),
        background_color: Some(color),
        ..HighlightStyle::default()
    }
}

/// Splits a path into its directory, dimmed, and its file name, bright.
fn split_path(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(slash) => path.split_at(slash + 1),
        None => ("", path),
    }
}

/// Syntax highlight styles for the runs of one line that lie within its first `len` bytes.
fn syntax_spans(theme: &Theme, spans: &[Span], len: usize) -> Vec<(Range<usize>, HighlightStyle)> {
    spans
        .iter()
        .filter(|(range, _)| range.end <= len)
        .filter_map(|(range, tone)| {
            let color = theme.syntax(*tone)?;
            Some((
                range.clone(),
                HighlightStyle {
                    color: Some(color),
                    ..HighlightStyle::default()
                },
            ))
        })
        .collect()
}

/// The parts of a line of code, which a row across the width and a half of a pair arrange.
pub(super) struct CellParts {
    /// The line number.
    pub gutter: Div,
    /// The `+`, `−` or blank sign.
    pub sign: Div,
    /// The code with its highlights.
    pub text: StyledText,
    /// The quiet `⏎` that stands for a blank changed line, so it never reads as an empty bar.
    pub blank: Option<Div>,
    /// The line's background, for changed lines in a style with line backgrounds.
    pub background: Option<Hsla>,
    /// The text color, for changed lines in a style without syntax colors.
    pub text_color: Option<Hsla>,
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

/// A line's number in its file, with the change bar along the gutter's edge when there is one.
fn gutter(cell: &Cell, column: Column, bar: Option<Hsla>, colors: &Palette) -> Div {
    let nested = matches!(
        cell.kind,
        RowKind::NestedOld | RowKind::NestedNew | RowKind::NestedSame
    );
    // Each half of a pair numbers its own side: the left the base (or a nested diff's old
    // side), the right the head. Across the width, a nested line has no single number.
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
    div()
        .w(px(super::present::GUTTER))
        .h_full()
        .flex_none()
        .relative()
        .flex()
        .items_center()
        .justify_end()
        .pr(px(GRID * 2.0))
        .children(bar.map(|color| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .right(px(GRID * 0.5))
                .w(px(2.0))
                .bg(color)
        }))
        .text_color(if cell.kind == RowKind::Removed {
            Hsla {
                a: 0.6,
                ..colors.faint
            }
        } else {
            colors.faint
        })
        .child(number)
}

impl ReviewApp {
    /// The look on screen: the preview of the settings list, or the applied look.
    pub(super) fn look(&self) -> &Look {
        self.settings.appearance.shown()
    }

    /// The current theme.
    pub(super) fn theme(&self) -> &Theme {
        self.look().theme.as_ref()
    }

    /// The palette of the current theme.
    pub(super) fn palette(&self) -> Palette {
        Palette::new(
            self.theme(),
            self.settings.removed,
            self.settings.added,
            self.look().style.background,
        )
    }

    /// The diff row to measure for the list's width: the widest code row or the widest
    /// annotation, whichever is wider in pixels at their font sizes and indents. Both fonts are
    /// the same monospace family, so a cell is a fixed share of the font size.
    fn widest_row(&self) -> Option<usize> {
        const CELL: f32 = 0.6;
        let code_indent = FRAME + GRID * 13.0 + GRID * 4.0;
        let note_indent = FRAME + GRID * 17.0;
        let pixels =
            |indent: f32, cells: usize, size: f32| indent + scroll::float(cells) * size * CELL;
        let code = self
            .rows
            .widest_code
            .map(|(row, cells)| (row, pixels(code_indent, cells, self.fonts.code_size)));
        let note = self
            .rows
            .widest_annotation
            .map(|(row, cells)| (row, pixels(note_indent, cells, self.fonts.ui_size())));
        match (code, note) {
            (Some((code, code_width)), Some((note, note_width))) => {
                Some(if note_width > code_width { note } else { code })
            }
            (one, other) => one.or(other).map(|(row, _)| row),
        }
    }

    /// Whether diff row `index` is in a hunk, from its header to its last row.
    fn in_hunk(&self, index: usize) -> bool {
        // Hunks are in row order, so the one that may hold the row is the last starting at or
        // before it.
        let after = self.hunk_rows.partition_point(|rows| rows.start <= index);
        after
            .checked_sub(1)
            .and_then(|hunk| self.hunk_rows.get(hunk))
            .is_some_and(|rows| rows.contains(&index))
    }

    /// The rows of the diff list, built on demand by the virtualized list.
    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let colors = self.palette();
        let row_height = self.fonts.row_height();
        let selected = self
            .hunk_rows
            .get(self.hunk)
            .cloned()
            .filter(|_| self.mode == Mode::Diff);
        let rows: Vec<AnyElement> = range
            .filter_map(|index| {
                let row = self.rows.rows.get(index)?;
                let frame = if selected.as_ref().is_some_and(|rows| rows.contains(&index)) {
                    Frame::Selected
                } else if self.in_hunk(index) {
                    Frame::Hunk
                } else {
                    Frame::Outside
                };
                let element = match &row.body {
                    Body::Code(cell) => self
                        .code_row(index, row.file, cell, frame, &colors)
                        .h(px(row_height))
                        .into_any_element(),
                    Body::Pair(pair) => self.pair_row(index, row.file, pair, frame, &colors),
                    Body::Note(note) => self
                        .annotation_row(index, row, note, frame, &colors, cx)
                        .h(px(row_height))
                        .into_any_element(),
                };
                Some(element)
            })
            .collect();
        if let Some(bench) = &mut self.bench {
            bench.rendered_rows += rows.len();
            bench.built = bench.render_started.map(|started| started.elapsed());
        }
        rows
    }

    /// The highlight layers of a line of code painted in `column`: syntax colors, the diff
    /// search's matches in that column, then the selection when the line takes part in it.
    fn code_layers(
        &self,
        index: usize,
        file: usize,
        cell: &Cell,
        column: Column,
    ) -> Vec<Vec<(Range<usize>, HighlightStyle)>> {
        let mut layers = vec![self.syntax_layer(file, cell)];
        if let Some(find) = self.search_of(Mode::Diff) {
            let colors = self.palette();
            let current = find.current.and_then(|current| find.matches.get(current));
            let start = find.matches.partition_point(|found| found.row < index);
            layers.push(
                find.matches
                    .get(start..)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|found| found.row == index)
                    .filter(|found| found.column == column)
                    .map(|found| {
                        let style = if current == Some(found) {
                            block(colors.accent, colors.background)
                        } else {
                            background(colors.find)
                        };
                        (found.range.clone(), style)
                    })
                    .collect(),
            );
        }
        if let Some(range) = self
            .selection
            .filter(|selection| selection.pane == Pane::Diff && selection.covers(column))
            .and_then(|selection| selection.range_in_row(index, cell.content_len))
        {
            layers.push(vec![(range, background(self.palette().selection))]);
        }
        layers
    }

    /// The syntax colors of a line of code, in a style with syntax colors.
    ///
    /// A style without them, as lazygit draws a diff, has none anywhere, only red and green
    /// changed lines.
    /// A line of the head takes the head's colors, any other line of the base the base's.
    fn syntax_layer(&self, file: usize, cell: &Cell) -> Vec<(Range<usize>, HighlightStyle)> {
        let style = &self.look().style;
        if !style.syntax {
            return Vec::new();
        }
        let Some(syntax) = self
            .rows
            .files
            .get(file)
            .and_then(|section| self.folds.syntax.get(&section.path))
        else {
            return Vec::new();
        };
        // A nested line's old side comes from another file or version than this file's base,
        // whose colors it never takes.
        let nested = matches!(
            cell.kind,
            RowKind::NestedOld | RowKind::NestedNew | RowKind::NestedSame
        );
        let spans = match (&cell.head, cell.base) {
            (Some(head), _) => syntax.head.as_ref().map(|side| side.line(head.line)),
            (None, Some(base)) if !nested => syntax.base.as_ref().map(|side| side.line(base)),
            (None, _) => None,
        };
        let mut layer = syntax_spans(self.theme(), spans.unwrap_or_default(), cell.content_len);
        // On a tinted line, dim colors such as comments lift toward the text color until they
        // stay readable against the line's final background.
        let colors = self.palette();
        let tint = match cell.kind {
            _ if style.background == Background::None => None,
            _ if cell.quiet => Some(colors.quiet_line),
            RowKind::Removed | RowKind::NestedOld => Some(colors.removed_line),
            RowKind::Added | RowKind::NestedNew => Some(colors.added_line),
            _ => None,
        };
        if let Some(tint) = tint {
            for (_, style) in &mut layer {
                style.color = style
                    .color
                    .map(|color| crate::theme::legible(color, tint, colors.text));
            }
        }
        layer
    }

    /// How the diff style inks a line of code: its sign, background, text color and change
    /// bar.
    ///
    /// A formatting-only line is shown, but never as a removal or an addition.
    fn cell_ink(&self, cell: &Cell, colors: &Palette) -> CellInk {
        let style = &self.look().style;
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
                    sign_color: colors.faint,
                    background: line(colors.quiet_line),
                    text_color: None,
                    bar: bar(colors.faint),
                }
            }
            RowKind::Removed | RowKind::NestedOld => CellInk {
                sign: "−",
                sign_color: colors.removed_sign,
                background: line(colors.removed_line),
                text_color: text(colors.removed_text),
                bar: bar(colors.removed_sign),
            },
            RowKind::Added | RowKind::NestedNew => CellInk {
                sign: "+",
                sign_color: colors.added_sign,
                background: line(colors.added_line),
                text_color: text(colors.added_text),
                bar: bar(colors.added_sign),
            },
            _ => CellInk {
                sign: " ",
                sign_color: colors.faint,
                background: None,
                text_color: None,
                bar: None,
            },
        }
    }

    /// The underlines of the tokens a line really changed, when the look emphasizes them.
    fn emphasis_layer(&self, cell: &Cell, colors: &Palette) -> Vec<(Range<usize>, HighlightStyle)> {
        if !self.look().token_emphasis || cell.quiet {
            return Vec::new();
        }
        let underline = HighlightStyle {
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(Hsla {
                    a: 0.5,
                    ..colors.text
                }),
                wavy: false,
            }),
            ..HighlightStyle::default()
        };
        cell.highlights
            .iter()
            .map(|range| (range.clone(), underline))
            .collect()
    }

    /// A line of code's parts, painted in `column`: the gutter, the sign, the highlighted text
    /// and the line's colors; and the text's layout, for mapping the mouse to it.
    pub(super) fn code_cell(
        &self,
        index: usize,
        file: usize,
        cell: &Cell,
        column: Column,
        colors: &Palette,
    ) -> (CellParts, gpui::TextLayout) {
        let ink = self.cell_ink(cell, colors);
        let mut layers = self.code_layers(index, file, cell, column);
        layers.push(self.emphasis_layer(cell, colors));
        let highlights = merge_highlights(cell.text.len(), &layers);
        let text = StyledText::new(cell.text.clone()).with_highlights(highlights);
        let layout = text.layout().clone();
        let blank = (cell.text.is_empty()
            && matches!(cell.kind, RowKind::Removed | RowKind::Added))
        .then(|| div().text_color(colors.faint).child("⏎"));
        let parts = CellParts {
            gutter: gutter(cell, column, ink.bar, colors),
            sign: div()
                .w(px(super::present::SIGN))
                .flex_none()
                .text_color(ink.sign_color)
                .child(ink.sign),
            text,
            blank,
            background: ink.background,
            text_color: ink.text_color,
        };
        (parts, layout)
    }

    /// A line of code across the width: the gutter, the sign and the text, on the line's
    /// background.
    fn code_row(
        &self,
        index: usize,
        file: usize,
        cell: &Cell,
        frame: Frame,
        colors: &Palette,
    ) -> Div {
        let (parts, layout) = self.code_cell(index, file, cell, Column::Whole, colors);
        self.painted
            .borrow_mut()
            .insert((Pane::Diff, Column::Whole, index), layout);
        h_flex()
            .min_w_full()
            .whitespace_nowrap()
            .when_some(parts.background, gpui::Styled::bg)
            .when_some(parts.text_color, gpui::Styled::text_color)
            .child(frame_bar(frame, colors))
            .child(parts.gutter)
            .child(parts.sign)
            .child(self.shifted(h_flex().child(parts.text).children(parts.blank)))
    }

    /// One of our own rows: a hunk header, a gap, a note, a move or a description, on the
    /// annotation band, in the window's font size, without a gutter.
    fn annotation_row(
        &self,
        index: usize,
        row: &Row,
        note: &Note,
        frame: Frame,
        colors: &Palette,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let small = self.fonts.ui_size();
        let base = div()
            .id(("row", index))
            .min_w_full()
            .whitespace_nowrap()
            .text_size(px(small))
            .flex()
            .items_center();
        match note.kind {
            RowKind::Spacer => base,
            RowKind::Gap => base
                .cursor_pointer()
                .child(frame_bar(frame, colors))
                .child(div().w(px(GRID * 17.0)).flex_none())
                .child(
                    self.clipped(
                        h_flex()
                            .flex_1()
                            .child(div().w(px(GRID * 6.0)).h(px(1.0)).bg(colors.hairline))
                            .child(
                                div()
                                    .px(px(GRID * 2.0))
                                    .text_color(colors.faint)
                                    .child(note.text.clone()),
                            )
                            .child(div().flex_1().h(px(1.0)).bg(colors.hairline)),
                    ),
                )
                .hover(|row| row.bg(colors.band))
                .on_click(cx.listener(move |this, _, _, cx| this.expand_gap(index, cx))),
            RowKind::Header => self.hunk_header(row, note, frame, base, colors),
            RowKind::Note => base
                .bg(colors.band)
                .child(frame_bar(frame, colors))
                .child(div().w(px(GRID * 13.0)).flex_none())
                .child(div().w(px(3.0)).h_full().flex_none().bg(colors.accent))
                .child(
                    self.clipped(
                        h_flex()
                            .child(
                                div()
                                    .pl(px(GRID * 3.0))
                                    .text_color(colors.text)
                                    .child(note.text.clone()),
                            )
                            .children(note.tag.clone().map(|tag| {
                                div().pl(px(GRID * 3.0)).text_color(colors.faint).child(tag)
                            })),
                    ),
                ),
            _ => {
                let color = match note.kind {
                    RowKind::Moved => colors.moved,
                    _ => colors.dim,
                };
                let hint = match note.kind {
                    RowKind::Moved if note.link.is_some() => Some("⏎ follow"),
                    RowKind::Collapsed => Some("⏎ shows the code"),
                    _ => None,
                };
                base.bg(colors.band)
                    .child(frame_bar(frame, colors))
                    .child(div().w(px(GRID * 17.0)).flex_none())
                    .child(
                        self.clipped(
                            h_flex()
                                .child(div().text_color(color).child(note.text.clone()))
                                .children(hint.map(|hint| {
                                    div()
                                        .pl(px(GRID * 3.0))
                                        .text_color(colors.faint)
                                        .child(hint)
                                })),
                        ),
                    )
            }
        }
    }

    /// Whether `units` are all chunks of text, so their header's text is only where they are.
    fn all_chunks(&self, units: &[UnitId]) -> bool {
        self.view().is_some_and(|view| {
            !units.is_empty()
                && units.iter().all(|&unit| {
                    view.unit(unit)
                        .ok()
                        .and_then(|record| record.chunk())
                        .is_some()
                })
        })
    }

    /// A hunk's header: one quiet line with its state glyph, the scopes around it, what kind of
    /// change it is, and where it is when its code is folded away or no scope names it.
    ///
    /// It is a gray band, blue when selected; with chips headers its tag and location are small
    /// muted chips.
    fn hunk_header(
        &self,
        row: &Row,
        note: &Note,
        frame: Frame,
        base: Stateful<Div>,
        colors: &Palette,
    ) -> Stateful<Div> {
        let units = row
            .hunk
            .and_then(|hunk| self.rows.hunks.get(hunk))
            .cloned()
            .unwrap_or_default();
        let badges = self
            .view()
            .map(|view| state::badges(&view, &units, &self.pending))
            .unwrap_or_default();
        let (badge, glyph) = badges_glyph(&badges);
        let folded = self
            .view()
            .is_some_and(|view| units.iter().all(|&unit| self.folds.is_folded(&view, unit)));
        let selected = frame == Frame::Selected;
        let label = if folded && badge == Badge::Accepted {
            colors.faint
        } else {
            colors.dim
        };
        let chips = self.look().style.headers == Headers::Chips;
        let background = header_background(selected, colors);
        // What the hunk changes leads: the scopes around it, innermost brightest. Where it is
        // matters only when its code is folded away or no scope names it, since the gutter
        // numbers the lines that show; a file unit's text says what changed, so it stays.
        let receding = folded && badge == Badge::Accepted;
        let lines_only = self.all_chunks(&units);
        let scope = note
            .scope
            .as_deref()
            .map(|scope| scope_title(scope, receding, colors));
        let location = (scope.is_none() || folded || !lines_only).then(|| note.text.clone());
        let location_color = if scope.is_some() && lines_only {
            colors.faint
        } else {
            label
        };
        let content = if chips {
            let chip = |text: String, color: Hsla| {
                div()
                    .flex_none()
                    .px(px(GRID * 1.5))
                    .line_height(px(self.fonts.ui_size() * 1.45))
                    .bg(if selected {
                        colors.band
                    } else {
                        colors.surface
                    })
                    .text_color(color)
                    .child(text)
            };
            h_flex()
                .min_w_0()
                .gap(px(GRID * 2.0))
                .children(scope)
                .children(note.tag.clone().map(|tag| chip(tag, colors.kind_quiet)))
                .children(location.map(|text| chip(text, location_color)))
        } else {
            let after = |first: bool| if first { 0.0 } else { GRID * 2.0 };
            let has_scope = scope.is_some();
            h_flex()
                .min_w_0()
                .children(scope)
                .children(note.tag.clone().map(|tag| {
                    div()
                        .ml(px(after(!has_scope)))
                        .flex_none()
                        .text_color(colors.kind_quiet)
                        .child(if has_scope { format!("· {tag}") } else { tag })
                }))
                .children(location.map(|text| {
                    let first = !has_scope && note.tag.is_none();
                    div()
                        .ml(px(after(first)))
                        .flex_none()
                        .text_color(location_color)
                        .child(if first { text } else { format!("· {text}") })
                }))
        };
        base.bg(background)
            .child(frame_bar(frame, colors))
            .child(
                div()
                    .w(px(GRID * 13.0))
                    .flex_none()
                    .pr(px(GRID * 2.0))
                    .text_right()
                    .text_color(colors.faint)
                    .child(if folded { "▸" } else { "▾" }),
            )
            .child(
                div()
                    .w(px(GRID * 4.0))
                    .flex_none()
                    .text_color(colors.badge(badge))
                    .child(glyph),
            )
            .child(self.clipped(content))
    }

    /// The files pane: a header with the totals, and the tree.
    fn render_sidebar(&self, width: f32, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let (reviewed, total) = self.totals();
        let focused = self.mode == Mode::Tree && self.overlay.is_none();
        let files = self
            .tree
            .iter()
            .filter(|node| node.kind == NodeKind::File)
            .count();
        let visible = self.visible_tree();
        let count = visible.len();
        let filter = self.filter_query().map(|query| {
            let shown = visible
                .iter()
                .filter(|&&index| {
                    self.tree
                        .get(index)
                        .is_some_and(|node| node.kind == NodeKind::File)
                })
                .count();
            (format!("/{query}"), shown)
        });
        let title = match &filter {
            Some((_, shown)) => format!("Files {shown}/{files}"),
            None => format!("Files {files}"),
        };
        v_flex()
            .w(px(width))
            .h_full()
            .flex_none()
            .bg(colors.surface)
            .child(
                h_flex()
                    .h(px(BAR))
                    .flex_none()
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 2.0))
                    .border_b_1()
                    .border_color(colors.hairline)
                    .child(
                        div()
                            .flex_none()
                            .text_color(if focused { colors.text } else { colors.dim })
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(colors.accent)
                            .children(filter.map(|(query, _)| query)),
                    )
                    .child(progress_meter(reviewed, total, &colors)),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        uniform_list(
                            "tree",
                            count,
                            cx.processor(move |this, range: Range<usize>, _, cx| {
                                this.render_tree_rows(range, cx)
                            }),
                        )
                        .track_scroll(&self.tree_list)
                        .size_full(),
                    )
                    .child(Scrollbar::vertical(&self.tree_list).mode(ScrollbarMode::Always)),
            )
    }

    /// The visible tree rows in `range`: indent, a folder chevron, the change kind, the name
    /// colored by review state, and one state glyph.
    #[expect(
        clippy::too_many_lines,
        reason = "one declarative row layout with its click handler"
    )]
    fn render_tree_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let colors = self.palette();
        let visible = self.visible_tree();
        let focused = self.mode == Mode::Tree && self.overlay.is_none();
        let row_height = self.fonts.row_height();
        let cursor = self.tree_cursor.min(visible.len().saturating_sub(1));
        range
            .filter_map(|position| {
                let index = *visible.get(position)?;
                let node = self.tree.get(index)?;
                let directory = node.kind == NodeKind::Directory;
                let progress = Progress::of(&node.counts);
                let collapsed = self.tree_collapsed.contains(&node.path);
                let shown = !directory && self.selected.as_deref() == Some(node.path.as_str());
                let is_cursor = position == cursor;
                // Only what still needs the reviewer carries ink; done files recede, so the eye
                // lands on open, flagged and changed ones.
                let done = progress == Progress::Done;
                let kind = node.change.as_ref().map(|change| {
                    (
                        change.kind.letter(),
                        if done {
                            colors.faint
                        } else {
                            colors.tone(change.tone())
                        },
                        change.kind == review_core::ChangeKind::Deleted,
                    )
                });
                let (name_color, glyph_color) = if done {
                    (colors.dim, colors.faint)
                } else {
                    (colors.progress(progress), colors.progress(progress))
                };
                Some(
                    h_flex()
                        .id(("tree-node", index))
                        .h(px(row_height))
                        .w_full()
                        .pr(px(GRID * 3.0))
                        .pl(px(GRID * 2.0 + scroll::float(node.depth) * GRID * 4.0))
                        .gap(px(GRID))
                        .cursor_pointer()
                        .when(is_cursor && focused, |row| row.bg(colors.raised))
                        .when(shown && !(is_cursor && focused), |row| {
                            row.bg(Hsla {
                                a: 0.5,
                                ..colors.raised
                            })
                        })
                        .hover(|row| row.bg(colors.hairline))
                        .child(
                            div()
                                .w(px(GRID * 4.0))
                                .flex_none()
                                .text_color(colors.faint)
                                .child(match (directory, collapsed) {
                                    (true, true) => "▸",
                                    (true, false) => "▾",
                                    (false, _) => "",
                                }),
                        )
                        .child(
                            div()
                                .w(px(GRID * 4.0))
                                .flex_none()
                                .text_color(kind.map_or(colors.faint, |(_, color, _)| color))
                                .children(
                                    kind.map(|(letter, ..)| SharedString::from(letter.to_string())),
                                ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(if directory {
                                    Hsla {
                                        a: 0.8,
                                        ..name_color
                                    }
                                } else {
                                    name_color
                                })
                                .when(kind.is_some_and(|(_, _, deleted)| deleted), |name| {
                                    name.line_through()
                                })
                                .child(if directory {
                                    format!("{}/", node.name)
                                } else {
                                    node.name.clone()
                                }),
                        )
                        .child(
                            div()
                                .w(px(GRID * 4.0))
                                .flex_none()
                                .text_center()
                                .text_color(glyph_color)
                                .child(progress_glyph(progress)),
                        )
                        .on_click(
                            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                this.tree_cursor = position;
                                if directory {
                                    this.tree_fold(None, cx);
                                } else if let Some(path) =
                                    this.tree.get(index).map(|node| node.path.clone())
                                {
                                    this.show_file(&path);
                                    this.tree_cursor = position;
                                    if event.click_count() > 1 {
                                        this.enter_mode(Mode::Diff, window, cx);
                                    }
                                }
                                if this.mode != Mode::Tree && event.click_count() == 1 {
                                    this.enter_mode(Mode::Tree, window, cx);
                                }
                                cx.notify();
                            }),
                        )
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// The divider between the files and the diff: a hairline with a wider grip.
    fn render_divider(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let active = self.resizing;
        div()
            .id("sidebar-divider")
            .group("sidebar-divider")
            .relative()
            .flex_none()
            .w(px(GRID + 1.0))
            .h_full()
            .bg(colors.surface)
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .child(self.regions.mark("sidebar-divider", "ew-resize"))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(px(1.0))
                    .bg(if active {
                        colors.accent
                    } else {
                        colors.hairline
                    })
                    .group_hover("sidebar-divider", |line| line.bg(colors.accent)),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.press_divider(event.click_count, cx);
                }),
            )
    }

    /// The selected file's header: its change kind, path and details, and its progress.
    fn render_file_header(&self) -> impl IntoElement + use<> {
        let colors = self.palette();
        let Some(section) = self.rows.files.first() else {
            return h_flex()
                .h(px(BAR))
                .flex_none()
                .px(px(GRID * 3.0))
                .border_b_1()
                .border_color(colors.hairline)
                .text_color(colors.faint)
                .child("No file selected");
        };
        let (dir, name) = split_path(&section.path);
        let change = section.change.as_ref();
        let kind_color = change.map_or(colors.text, |change| colors.tone(change.tone()));
        let mut details: Vec<String> = Vec::new();
        if let Some(old) = &section.old_path {
            details.push(format!("renamed from {old}"));
        }
        if let Some(change) = change {
            details.extend(
                change
                    .entry_changes
                    .iter()
                    .map(|unit| format!("{unit:?}").to_lowercase()),
            );
            if change.formatting_only {
                details.push("only reformatting".to_owned());
            }
            if let Some(noise) = change.noise {
                details.push(noise.name().to_owned());
            }
            if change.moves {
                details.push("moved code".to_owned());
            }
        }
        if self.look().layout == crate::config::DiffLayout::SideBySide && !self.side_by_side() {
            details.push("unified · too narrow for side by side".to_owned());
        }
        let units: Vec<_> = self.rows.hunks.concat();
        let approved = self
            .view()
            .map(|view| {
                state::badges(&view, &units, &self.pending)
                    .into_iter()
                    .filter(|&badge| state::is_approved(badge))
                    .count()
            })
            .unwrap_or(0);
        h_flex()
            .h(px(BAR))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 3.0))
            .bg(colors.surface)
            .border_b_1()
            .border_color(colors.hairline)
            .children(change.map(|change| {
                div()
                    .text_color(kind_color)
                    .font_weight(FontWeight::BOLD)
                    .child(SharedString::from(change.kind.letter().to_string()))
            }))
            .child(
                h_flex()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().text_color(colors.dim).child(dir.to_owned()))
                    .child(
                        div()
                            .text_color(colors.text)
                            .font_weight(FontWeight::BOLD)
                            .child(name.to_owned()),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(colors.faint)
                    .child(details.join(" · ")),
            )
            .child(
                div()
                    .text_color(colors.added_sign)
                    .child(format!("+{}", section.added)),
            )
            .child(
                div()
                    .text_color(colors.removed_sign)
                    .child(format!("−{}", section.removed)),
            )
            .child(progress_meter(approved, units.len(), &colors))
    }

    /// The diff list of the selected file, with its scrollbars, or a loading or empty state.
    fn render_diff(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let row_height = self.fonts.row_height();
        let side_by_side = self.side_by_side();
        let list = uniform_list(
            "rows",
            self.rows.rows.len(),
            cx.processor(|this, range, _, cx| this.render_rows(range, cx)),
        )
        .track_scroll(&self.list);
        // Side by side, the halves move their text by the shared offset instead; see `pairs`.
        let list = if side_by_side {
            let viewport = gpui_component::scroll::ScrollbarHandle::viewport_bounds(&self.list);
            self.text_scroll.update(
                self.text_extent(window, f32::from(viewport.size.width)),
                viewport,
            );
            list.with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::FitList)
        } else {
            list.with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(self.widest_row())
        }
        .size_full();
        let empty = match &self.phase {
            Phase::Loading(started) => Some(state_card(
                "Opening the review",
                &format!("Reading history and analysis{}", dots(*started)),
                colors.dim,
                &colors,
            )),
            Phase::Failed(err) => Some(state_card(
                "Couldn't open this review",
                err,
                colors.error,
                &colors,
            )),
            // A previewed file's diff follows once the tree cursor rests.
            Phase::Ready if self.preview.is_some() => None,
            Phase::Ready if self.rows.rows.is_empty() => Some(state_card(
                "Nothing to show",
                "Pick a file on the left.",
                colors.dim,
                &colors,
            )),
            Phase::Ready => None,
        };
        div()
            .id("list-area")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .bg(colors.background)
            .text_size(px(self.fonts.code_size))
            .line_height(px(row_height))
            .cursor_text()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, window, cx| {
                    this.mouse_down(Pane::Diff, event, window, cx);
                }),
            )
            .on_scroll_wheel(cx.listener(Self::text_wheel))
            .child(list)
            .child(Scrollbar::vertical(&self.list_bar).mode(ScrollbarMode::Always))
            .map(|area| {
                if side_by_side {
                    area.child(Scrollbar::horizontal(&self.text_scroll))
                } else {
                    area.child(Scrollbar::horizontal(&self.list_bar))
                }
            })
            .children(empty.map(|card| {
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(card)
            }))
    }

    /// The buffer: the whole file with change markers in the gutter, the cursor line, find
    /// matches and the selection.
    #[expect(
        clippy::too_many_lines,
        reason = "the buffer's header, find bar and virtualized rows share their captured state"
    )]
    fn render_buffer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let colors = self.palette();
        let row_height = self.fonts.row_height();
        let Some(buffer) = self.buffer.as_ref() else {
            return div().into_any_element();
        };
        let theme = Arc::clone(&self.look().theme);
        let lines = Arc::clone(&buffer.lines);
        let selection = self
            .selection
            .filter(|selection| selection.pane == Pane::Source);
        let matches: Arc<Vec<super::find::Match>> = self
            .search_of(Mode::Buffer)
            .map(|find| Arc::clone(&find.matches))
            .unwrap_or_default();
        let current = self
            .search_of(Mode::Buffer)
            .and_then(|find| find.current)
            .and_then(|index| matches.get(index).cloned());
        let painted = Rc::clone(&self.painted);
        let cursor = buffer.cursor;
        let markers = Arc::clone(&buffer.markers);
        let syntax = buffer.syntax.clone();
        let longest = lines.longest;
        // The scopes around the cursor stay in the header as the buffer scrolls.
        let scope = buffer.scope();
        let header = h_flex()
            .h(px(BAR))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 3.0))
            .bg(colors.surface)
            .border_b_1()
            .border_color(colors.hairline)
            .child(
                h_flex()
                    .min_w_0()
                    .child(
                        div()
                            .flex_none()
                            .text_color(colors.text)
                            .font_weight(FontWeight::BOLD)
                            .child(buffer.title().to_owned()),
                    )
                    .children(scope.map(|scope| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(colors.dim)
                            .child(format!(" › {scope}"))
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(colors.faint)
                    .child(match &buffer.origin {
                        super::buffer::Origin::Review(_) => "whole file, read-only".to_owned(),
                        super::buffer::Origin::Definition(_) => {
                            "definition, read-only · Ctrl+O back".to_owned()
                        }
                        super::buffer::Origin::Head(_) => {
                            "not changed in this review, read-only".to_owned()
                        }
                    }),
            )
            .child(div().flex_1())
            .child(div().text_color(colors.dim).child(format!(
                "{}:{}",
                cursor.row + 1,
                cursor.col + 1
            )));
        let list = uniform_list("buffer", lines.lines.len(), move |range, _, _| {
            range
                .filter_map(|index| {
                    let line = lines.lines.get(index)?;
                    let mut layers = Vec::new();
                    if let Some(syntax) = &syntax {
                        layers.push(syntax_spans(&theme, syntax.line(index), line.len()));
                    }
                    let start = matches.partition_point(|found| found.row < index);
                    let found: Vec<_> = matches
                        .get(start..)
                        .unwrap_or_default()
                        .iter()
                        .take_while(|found| found.row == index)
                        .map(|found| {
                            let strong = current.as_ref() == Some(found);
                            (
                                found.range.clone(),
                                if strong {
                                    block(colors.accent, colors.background)
                                } else {
                                    background(colors.find)
                                },
                            )
                        })
                        .collect();
                    layers.push(found);
                    if let Some(range) =
                        selection.and_then(|selection| selection.range_in_row(index, line.len()))
                    {
                        layers.push(vec![(range, background(colors.selection))]);
                    }
                    if index == cursor.row {
                        let end = line
                            .get(cursor.col..)
                            .and_then(|rest| rest.chars().next())
                            .map_or(cursor.col, |next| cursor.col + next.len_utf8());
                        if end > cursor.col {
                            layers.push(vec![(
                                cursor.col..end,
                                block(colors.text, colors.background),
                            )]);
                        }
                    }
                    let text = StyledText::new(line.clone())
                        .with_highlights(merge_highlights(line.len(), &layers));
                    painted
                        .borrow_mut()
                        .insert((Pane::Source, Column::Whole, index), text.layout().clone());
                    let marker = markers.get(index).copied().flatten();
                    Some(
                        h_flex()
                            .h(px(row_height))
                            .min_w_full()
                            .whitespace_nowrap()
                            .when(index == cursor.row, |row| row.bg(colors.raised))
                            .child(
                                div()
                                    .w(px(3.0))
                                    .h_full()
                                    .flex_none()
                                    .when(matches!(marker, Some(Marker::Added(_))), |bar| {
                                        bar.bg(colors.added_sign)
                                    })
                                    .when(matches!(marker, Some(Marker::Removed(_))), |bar| {
                                        bar.border_t_2().border_color(colors.removed_sign)
                                    }),
                            )
                            .child(
                                div()
                                    .w(px(GRID * 13.0))
                                    .flex_none()
                                    .pr(px(GRID * 3.0))
                                    .text_right()
                                    .text_color(if index == cursor.row {
                                        colors.text
                                    } else {
                                        colors.faint
                                    })
                                    .child(format!("{}", index + 1)),
                            )
                            .child(text)
                            .into_any_element(),
                    )
                })
                .collect()
        })
        .track_scroll(&self.buffer_list)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(longest))
        .size_full();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(header)
            .child(
                div()
                    .id("buffer-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .bg(colors.background)
                    .text_size(px(self.fonts.code_size))
                    .line_height(px(row_height))
                    .cursor_text()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event, window, cx| {
                            this.mouse_down(Pane::Source, event, window, cx);
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, _: &ScrollWheelEvent, _, _| {
                        // The wheel takes over from a scroll animation, which would
                        // otherwise pull the buffer back to its target on the next frames.
                        this.buffer_animation = None;
                        this.buffer_bar.cancel_page_request();
                    }))
                    .child(list)
                    .child(Scrollbar::vertical(&self.buffer_bar).mode(ScrollbarMode::Always))
                    .child(Scrollbar::horizontal(&self.buffer_bar)),
            )
            .into_any_element()
    }

    /// The `/` bar of the current mode, outside the modes' key context so typing never
    /// triggers their keys: the tree's filter, or the diff's or the buffer's find.
    fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let colors = self.palette();
        if !self.typing() {
            return None;
        }
        let (input, count) = if self.mode == Mode::Tree {
            let filter = self.filter.as_ref()?;
            let files = self
                .visible_tree()
                .iter()
                .filter(|&&index| {
                    self.tree
                        .get(index)
                        .is_some_and(|node| node.kind == NodeKind::File)
                })
                .count();
            (filter.input.clone(), super::plural(files, "file"))
        } else {
            let find = self.search_of(self.mode)?;
            let count = match find.current {
                Some(current) => format!("{}/{}", current + 1, find.matches.len()),
                None if find.matches.len() == 1 => "1 match".to_owned(),
                None => format!("{} matches", find.matches.len()),
            };
            (find.input.clone(), count)
        };
        Some({
            h_flex()
                .key_context(Area::Find.context())
                .on_action(cx.listener(Self::close_find))
                .h(px(BAR))
                .flex_none()
                .px(px(GRID * 3.0))
                .gap(px(GRID * 2.0))
                .bg(colors.surface)
                .border_b_1()
                .border_color(colors.hairline)
                .child(div().text_color(colors.accent).child("/"))
                .child(div().flex_1().child(Input::new(&input).appearance(false)))
                .child(div().text_color(colors.dim).child(count))
        })
    }

    /// What the review keys act on now, for the bottom bar.
    fn target_label(&self) -> String {
        let file = self.selected.clone().unwrap_or_default();
        match self.mode {
            Mode::Tree => self
                .tree_node()
                .map(|node| match node.kind {
                    NodeKind::Directory => format!("{}/", node.path),
                    NodeKind::File => node.path.clone(),
                })
                .unwrap_or_default(),
            Mode::Diff if self.rows.hunks.is_empty() => file,
            Mode::Diff => format!("hunk {}/{} · {file}", self.hunk + 1, self.rows.hunks.len()),
            Mode::Buffer => {
                let line = self
                    .buffer
                    .as_ref()
                    .map_or(0, |buffer| buffer.cursor.row + 1);
                match self.buffer_hunk() {
                    Some(hunk) => format!(
                        "hunk {}/{} · line {line} · {file}",
                        hunk + 1,
                        self.rows.hunks.len()
                    ),
                    None => format!("line {line} · {file}"),
                }
            }
        }
    }

    /// Whether the bar should offer `action` now.
    fn hint_available(&self, action: &str) -> bool {
        match action {
            "NextFile" => !self.file_open() && self.next_open_file().is_some(),
            "GoToDefinition" => self.trusted,
            "ToggleApprove" | "ToggleFlag" => match self.mode {
                Mode::Buffer => self.buffer_hunk().is_some(),
                Mode::Diff => !self.rows.hunks.is_empty(),
                Mode::Tree => true,
            },
            "Dismiss" => self.mode == Mode::Buffer,
            "ToggleDiffStyle" => self.mode != Mode::Buffer,
            _ => true,
        }
    }

    /// The hints of the bottom bar that fit the window, in rank order, each with the label it
    /// shows. When the window is narrow, the highest-ranked hints make way whole; which hints
    /// show depends only on the mode, the diff style's name and the width, so nothing moves
    /// as the selection changes.
    fn bar_hints(&self, window: &Window) -> Vec<(super::keymap::Hint, String)> {
        let area = self.area();
        let mut hints = super::keymap::hints(&self.settings.keymap, area, |action| {
            self.hint_available(action)
        });
        // While a search is in the diff, `n` steps through its matches.
        if area == Area::Diff && self.diff_search_active() {
            for hint in hints.iter_mut().filter(|hint| hint.action == "NextFile") {
                hint.label = "next match";
                hint.available = true;
            }
        }
        // `|` says which diff style shows, the one it moves on from.
        let style = self.look().style.name.clone();
        let label = |hint: &super::keymap::Hint| match hint.action {
            "ToggleDiffStyle" => style.clone(),
            _ => hint.label.to_owned(),
        };
        let small = self.fonts.ui_size();
        let text_system = window.text_system();
        let cell = text_system
            .em_advance(text_system.resolve_font(&self.fonts.code), px(small))
            .map_or(small * 0.6, f32::from);
        let cells = |text: &str| scroll::float(crate::rows::display_width(text)) * cell;
        // The padding, the target's reserved width and the divider; a little slack covers
        // glyphs a fallback font draws wider.
        let reserved = GRID * 6.0 + GRID * 4.0 * 2.0 + 1.0 + (cell * 28.0).min(GRID * 120.0);
        let budget = (f32::from(window.viewport_size().width) - reserved) * 0.97;
        let kept = super::keymap::fit(
            &hints,
            budget,
            |hint| cells(&hint.keys) + GRID * 1.5 + cells(&label(hint)),
            GRID * 4.0,
        );
        hints
            .into_iter()
            .enumerate()
            .filter(|(index, _)| kept.contains(index))
            .map(|(_, hint)| {
                let label = label(&hint);
                (hint, label)
            })
            .collect()
    }

    /// The bottom bars: the status, then what the keys act on at the left and the keys of
    /// this mode at the right, `? keys` last at the edge.
    fn render_bars(&self, window: &mut Window) -> impl IntoElement + use<> {
        let colors = self.palette();
        let small = self.fonts.ui_size();
        let hints = self.bar_hints(window);
        let failing = self
            .failed_at
            .map(|at| 1.0 - scroll::ease_out(at.elapsed().as_secs_f32() / FLASH.as_secs_f32()));
        let newer = self
            .snapshot
            .as_ref()
            .and_then(super::state::Snapshot::newer);
        v_flex()
            .flex_none()
            .text_size(px(small))
            .child(
                h_flex()
                    .h(px(BAR - GRID))
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 4.0))
                    .bg(match failing {
                        Some(glow) => Hsla {
                            a: 0.25 * glow.max(0.0),
                            ..colors.error
                        },
                        None => colors.surface,
                    })
                    .border_t_1()
                    .border_color(colors.hairline)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if self.failed_at.is_some() {
                                colors.error
                            } else {
                                colors.dim
                            })
                            .child(self.status.clone()),
                    )
                    .children(newer.map(|newer| {
                        div()
                            .px(px(GRID * 2.0))
                            .bg(colors.accent)
                            .text_color(colors.background)
                            .child(format!("revision {newer} available · R switches"))
                    })),
            )
            .child(
                h_flex()
                    .h(px(BAR - GRID))
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 4.0))
                    .bg(colors.surface)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(colors.text)
                            .child(self.target_label()),
                    )
                    .child(div().w(px(1.0)).h(px(GRID * 4.0)).bg(colors.hairline))
                    .children(hints.into_iter().map(|(hint, label)| {
                        let (key_color, label_color) = if hint.available {
                            (colors.accent, colors.dim)
                        } else {
                            (colors.faint, colors.faint)
                        };
                        h_flex()
                            .flex_none()
                            .gap(px(GRID * 1.5))
                            .child(div().text_color(key_color).child(hint.keys))
                            .child(div().text_color(label_color).child(label))
                    })),
            )
    }

    /// The `?` cheat sheet: every bound action by section, from the keymap in effect.
    fn render_help(&self) -> impl IntoElement + use<> {
        let colors = self.palette();
        let small = self.fonts.ui_size();
        let columns = super::keymap::columns(super::keymap::sheet(&self.settings.keymap), 4);
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(Hsla {
                a: 0.6,
                ..colors.background
            })
            .child(
                v_flex()
                    .id("help-sheet")
                    .w(px(GRID * 360.0))
                    .max_w(relative(0.94))
                    .max_h_full()
                    .overflow_y_scroll()
                    .p(px(GRID * 6.0))
                    .gap(px(GRID * 4.0))
                    .bg(colors.surface)
                    .border_1()
                    .border_color(colors.hairline)
                    .text_size(px(small))
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                div()
                                    .text_color(colors.text)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Keyboard shortcuts"),
                            )
                            .child(
                                div()
                                    .text_color(colors.faint)
                                    .child("[keys] in config.toml rebinds · Esc closes"),
                            ),
                    )
                    .child(h_flex().items_start().gap(px(GRID * 8.0)).children(
                        columns.into_iter().map(|column| {
                            v_flex().flex_1().min_w_0().gap(px(GRID * 4.0)).children(
                                column.into_iter().map(|(group, entries)| {
                                    v_flex()
                                        .child(
                                            div()
                                                .h(px(GRID * 6.0))
                                                .mb(px(GRID))
                                                .border_b_1()
                                                .border_color(colors.hairline)
                                                .text_color(colors.dim)
                                                .child(group.to_uppercase()),
                                        )
                                        .children(entries.into_iter().map(|(keys, help)| {
                                            // Help wraps rather than hiding its end.
                                            h_flex()
                                                .items_start()
                                                .min_h(px(GRID * 6.0))
                                                .gap(px(GRID * 2.0))
                                                .child(
                                                    div()
                                                        .w(px(GRID * 26.0))
                                                        .flex_none()
                                                        .truncate()
                                                        .text_color(colors.accent)
                                                        .child(keys),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_color(colors.text)
                                                        .child(help),
                                                )
                                        }))
                                }),
                            )
                        }),
                    )),
            )
    }
}

use std::rc::Rc;

/// A hunk header's scopes, such as `impl Ledger › fn apply`: the outer ones faint and cut first
/// when space runs out, the innermost bright, or dim once the hunk is approved and folded.
fn scope_title(scope: &str, receding: bool, colors: &Palette) -> Div {
    let (outer, inner) = match scope.rsplit_once(" › ") {
        Some((outer, inner)) => (Some(format!("{outer} › ")), inner),
        None => (None, scope),
    };
    h_flex()
        .min_w_0()
        .whitespace_nowrap()
        .children(outer.map(|outer| {
            div()
                .min_w_0()
                .truncate()
                .text_color(colors.faint)
                .child(outer)
        }))
        .child(
            div()
                .flex_none()
                .text_color(if receding { colors.dim } else { colors.text })
                .child(inner.to_owned()),
        )
}

/// Where a diff row is relative to the hunks, for the bar along its left edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Frame {
    /// Between hunks: no bar.
    Outside,
    /// In a hunk that is not selected: a gray bar, so every hunk's extent shows.
    Hunk,
    /// In the selected hunk: the accent's bar.
    Selected,
}

/// The bar along the left edge of a hunk's rows: gray, or the accent in the selected hunk.
pub(super) fn frame_bar(frame: Frame, colors: &Palette) -> Div {
    let bar = div().w(px(FRAME)).h_full().flex_none();
    match frame {
        Frame::Outside => bar,
        Frame::Hunk => bar.bg(colors.faint),
        Frame::Selected => bar.bg(colors.accent),
    }
}

/// A hunk header's background: a gray band, so each hunk's start shows at a glance, and the
/// accent's blue on the selected one.
fn header_background(selected: bool, colors: &Palette) -> Hsla {
    if selected {
        Hsla {
            a: 0.3,
            ..colors.accent
        }
    } else {
        colors.band
    }
}

/// A reviewed/total meter with tabular counts, so it never shifts as numbers change.
pub fn progress_meter(reviewed: usize, total: usize, colors: &Palette) -> Div {
    let fraction = if total == 0 {
        0.0
    } else {
        scroll::float(reviewed) / scroll::float(total)
    };
    let width = 48.0;
    h_flex()
        .flex_none()
        .gap(px(GRID * 2.0))
        .child(
            div().w(px(width)).h(px(GRID)).bg(colors.hairline).child(
                div()
                    .h_full()
                    .w(px(width * fraction))
                    .bg(if reviewed == total && total > 0 {
                        colors.accepted
                    } else {
                        colors.accent
                    }),
            ),
        )
        .child(
            div()
                .min_w(px(GRID * 12.0))
                .text_right()
                .text_color(colors.dim)
                .child(format!("{reviewed}/{total}")),
        )
}

/// A centered card for loading, empty and error states.
fn state_card(
    title: &str,
    detail: &str,
    color: Hsla,
    colors: &Palette,
) -> impl IntoElement + use<> {
    v_flex()
        .max_w(px(560.0))
        .p(px(GRID * 6.0))
        .gap(px(GRID * 2.0))
        .bg(colors.surface)
        .border_1()
        .border_color(colors.hairline)
        .child(
            div()
                .text_color(colors.text)
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.to_owned()),
        )
        .child(div().text_color(color).child(detail.to_owned()))
}

/// Animated dots for a loading state.
fn dots(started: Instant) -> &'static str {
    match (started.elapsed().as_millis() / 400) % 4 {
        0 => "",
        1 => ".",
        2 => "..",
        _ => "...",
    }
}

impl Render for ReviewApp {
    #[expect(
        clippy::too_many_lines,
        reason = "one declarative layout with every action handler; splitting it would scatter them"
    )]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(bench) = &mut self.bench {
            bench.render_started = Some(Instant::now());
        }
        self.painted.borrow_mut().clear();
        self.regions.begin_frame();
        self.follow_width(window);
        if self.step_animations()
            || matches!(self.phase, Phase::Loading(_))
            || self.failed_at.is_some_and(|at| at.elapsed() < FLASH)
        {
            window.request_animation_frame();
        }
        self.note_frame(window, cx);
        let colors = self.palette();
        let client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
        self.client_decorated = client_decorated;
        let sidebar_width = self.shown_sidebar_width(window);
        let sidebar = self.sidebar_shown().then(|| {
            (
                self.render_sidebar(sidebar_width, cx),
                self.render_divider(cx),
            )
        });
        let right = if self.mode == Mode::Buffer && self.buffer.is_some() {
            self.render_buffer(cx).into_any_element()
        } else {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.render_file_header())
                .child(self.render_diff(window, cx))
                .into_any_element()
        };
        let bars = self.render_bars(window);
        let find_bar = self.render_find_bar(cx);
        let overlay = match self.overlay {
            Some(Overlay::Help) => Some(self.render_help().into_any_element()),
            Some(Overlay::Overview) => Some(
                div()
                    .absolute()
                    .inset_0()
                    .bg(colors.background)
                    .child(self.render_overview(cx))
                    .into_any_element(),
            ),
            // The picker and the settings list draw apart, in their own key contexts.
            Some(Overlay::Picker | Overlay::Settings) | None => None,
        };
        let mode_context = match self.mode {
            Mode::Tree => Area::Tree,
            Mode::Diff => Area::Diff,
            Mode::Buffer => Area::Buffer,
        };
        let mode_focus = match self.mode {
            Mode::Tree => self.tree_focus.clone(),
            Mode::Diff => self.diff_focus.clone(),
            Mode::Buffer => self.buffer_focus.clone(),
        };
        let content = v_flex()
            .key_context(Area::Window.context())
            .font(self.fonts.code.clone())
            .on_action(cx.listener(Self::toggle_trust))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::zoom_reset))
            .on_action(|_: &super::ToggleFullscreen, window, _| window.toggle_fullscreen())
            .on_action(|_: &super::CloseWindow, _, cx| cx.quit())
            .on_action(|_: &super::Quit, _, cx| cx.quit())
            .capture_key_down(
                cx.listener(|this, _, _, _| this.latency.pending = Some(Instant::now())),
            )
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.mouse_up(cx)),
            )
            .size_full()
            .bg(colors.background)
            .text_color(colors.text)
            .text_size(px(self.fonts.ui_size()))
            .when(client_decorated, |content| {
                content.child(super::chrome::title_bar(
                    self.title().into(),
                    window,
                    &self.regions,
                    &colors,
                ))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .key_context(mode_context.context())
                    .track_focus(&mode_focus)
                    .on_action(cx.listener(Self::tree_down))
                    .on_action(cx.listener(Self::tree_up))
                    .on_action(cx.listener(Self::tree_collapse))
                    .on_action(cx.listener(Self::tree_expand))
                    .on_action(cx.listener(Self::focus_diff))
                    .on_action(cx.listener(Self::close_all_folds))
                    .on_action(cx.listener(Self::open_all_folds))
                    .on_action(cx.listener(|this, action: &super::Activate, window, cx| {
                        match this.mode {
                            Mode::Tree => this.tree_activate(action, window, cx),
                            Mode::Diff => this.diff_activate(action, window, cx),
                            Mode::Buffer => {}
                        }
                    }))
                    .on_action(cx.listener(Self::next_hunk))
                    .on_action(cx.listener(Self::previous_hunk))
                    .on_action(cx.listener(Self::first_hunk))
                    .on_action(cx.listener(Self::last_hunk))
                    .on_action(cx.listener(Self::half_page_down))
                    .on_action(cx.listener(Self::half_page_up))
                    .on_action(cx.listener(Self::toggle_fold))
                    .on_action(cx.listener(Self::next_file))
                    .on_action(cx.listener(Self::focus_tree))
                    .on_action(cx.listener(Self::toggle_buffer))
                    .on_action(cx.listener(Self::toggle_approve))
                    .on_action(cx.listener(Self::toggle_flag))
                    .on_action(cx.listener(Self::reset_review))
                    .on_action(cx.listener(Self::refresh))
                    .on_action(cx.listener(Self::yank_unit_id))
                    .on_action(cx.listener(Self::cursor_down))
                    .on_action(cx.listener(Self::cursor_up))
                    .on_action(cx.listener(Self::next_word))
                    .on_action(cx.listener(Self::previous_word))
                    .on_action(cx.listener(Self::buffer_top))
                    .on_action(cx.listener(Self::buffer_bottom))
                    .on_action(cx.listener(Self::open_find))
                    .on_action(cx.listener(Self::clear_filter))
                    .on_action(cx.listener(Self::find_files))
                    .on_action(cx.listener(Self::search_content))
                    .on_action(cx.listener(Self::find_next))
                    .on_action(cx.listener(Self::find_previous))
                    .on_action(cx.listener(Self::go_to_definition))
                    .on_action(cx.listener(Self::jump_back))
                    .on_action(cx.listener(Self::buffer_dismiss))
                    .on_action(cx.listener(Self::toggle_overview))
                    .on_action(cx.listener(Self::toggle_diff_style))
                    .on_action(cx.listener(Self::open_settings))
                    .on_action(cx.listener(Self::toggle_layout))
                    .on_action(cx.listener(Self::toggle_approved))
                    .on_action(cx.listener(Self::screen_next))
                    .on_action(cx.listener(Self::screen_previous))
                    .on_action(cx.listener(Self::toggle_help))
                    .on_action(cx.listener(Self::copy))
                    .children(sidebar.map(|(sidebar, divider)| {
                        h_flex().h_full().flex_none().child(sidebar).child(divider)
                    }))
                    .child(right),
            )
            .children(find_bar)
            .child(bars)
            .children(overlay.map(|overlay| {
                // The overlay owns the pointer as well as the keyboard: nothing under it reacts.
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .key_context(Area::Overlay.context())
                    .track_focus(&self.overlay_focus)
                    .on_action(cx.listener(Self::close_overlay))
                    .child(overlay)
            }))
            .children(self.render_picker(cx).map(|picker| {
                // The picker owns the pointer and the keyboard too; its query or its list
                // takes the keys, each in its own context.
                let area = self.area();
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .key_context(area.context())
                    .track_focus(&self.overlay_focus)
                    .on_action(cx.listener(Self::picker_next))
                    .on_action(cx.listener(Self::picker_previous))
                    .on_action(cx.listener(Self::picker_open))
                    .on_action(cx.listener(Self::picker_scope))
                    .on_action(cx.listener(Self::picker_browse))
                    .on_action(cx.listener(Self::picker_edit))
                    .on_action(cx.listener(Self::picker_close))
                    .child(picker)
            }))
            .children(self.render_settings(window, cx));
        window_border().child(content)
    }
}

#[cfg(test)]
mod tests {
    use gpui::hsla;

    use super::*;

    /// Overlapping layers split into runs, and the later layer wins where both set a field.
    #[test]
    fn highlight_layers_merge_into_runs() {
        let red = HighlightStyle {
            color: Some(hsla(0.0, 1.0, 0.5, 1.0)),
            ..HighlightStyle::default()
        };
        let on_blue = background(hsla(0.6, 1.0, 0.5, 1.0));
        let merged = merge_highlights(10, &[vec![(0..6, red)], vec![(3..8, on_blue)]]);
        let ranges: Vec<Range<usize>> = merged.iter().map(|(range, _)| range.clone()).collect();
        assert_eq!(ranges, [0..3, 3..6, 6..8]);
        assert_eq!(merged[1].1.color, red.color);
        assert_eq!(merged[1].1.background_color, on_blue.background_color);
    }

    /// Many runs merge in one pass: a long minified line stays linear.
    #[test]
    fn many_runs_merge_quickly() {
        let red = HighlightStyle {
            color: Some(hsla(0.0, 1.0, 0.5, 1.0)),
            ..HighlightStyle::default()
        };
        let runs: Vec<(Range<usize>, HighlightStyle)> = (0..50_000)
            .map(|index| (index * 2..index * 2 + 1, red))
            .collect();
        let started = Instant::now();
        let merged = merge_highlights(
            100_000,
            &[
                runs,
                vec![(0..100_000, background(red.color.unwrap_or_default()))],
            ],
        );
        assert_eq!(merged.len(), 100_000);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn paths_split_into_directory_and_name() {
        assert_eq!(split_path("src/app/mod.rs"), ("src/app/", "mod.rs"));
        assert_eq!(split_path("README.md"), ("", "README.md"));
    }
}
