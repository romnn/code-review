//! The diff pane: the selected file's header, its rows, and the card of its loading, empty and
//! error states.
//!
//! A code row is review-gui's: the bar along a hunk's left edge (`▎`), the line number, the
//! sign and the code, on the line's background.
//! Side by side, each half has its own gutter and sign, a `│` divides them, padding is hatched,
//! and the code of every row moves by the model's text offset while gutters and the tool's own
//! rows stay put.
//! The tool's own rows (hunk headers, gaps, notes and moves) sit on their bands without a
//! gutter.

use std::time::Instant;

use ratatui::layout::Rect;
use review_ui::color::{Hsla, Rgba};
use review_ui::frame::List;
use review_ui::model::Model;
use review_ui::paint::annotation::{self, Annotation, HunkHeader, NotePan, Scope, TAG_GAP_CELLS};
use review_ui::paint::code::{self, At, CodeCell, Frame};
use review_ui::paint::display::DisplayLine;
use review_ui::paint::header::{self, FileHeader, Meter, StateCard};
use review_ui::paint::{Ink, Palette, Room};
use review_ui::rows::{Body, Cell, Pair, Row};
use review_ui::text::Column;

use super::files::meter_bar;
use super::painter::{Brush, Painter, ellipsize_end, ellipsize_start, width, wrap};
use super::{first_row, opaque};
use crate::color::over;
use crate::measure::{DIVIDER, FRAME, GUTTER, Panes, SIGN, halves};

/// The gap between the parts of the file header, in cells.
const HEADER_GAP: u16 = 2;

/// The cells of the rule before a gap's label.
const GAP_LEAD: u16 = 3;

/// The diff pane: the file header over the rows, or the card of a state without rows.
pub fn draw(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    panes: &Panes,
    now: Instant,
) {
    file_header(painter, model, palette, panes.header);
    let list = panes.list;
    let background = opaque(palette.background);
    painter.fill(list, background);
    let first = first_row(model, List::Diff);
    let text_offset = text_offset(model);
    for (offset, y) in (list.top()..list.bottom()).enumerate() {
        let index = first + offset;
        let Some(row) = model.rows.rows.get(index) else {
            break;
        };
        let at = RowAt {
            index,
            y,
            area: list,
            text_offset,
        };
        draw_row(painter, model, palette, row, at, background);
    }
    if let Some(card) = header::diff_state(model, palette, now) {
        state_card(painter, palette, list, &card);
    }
}

/// How far the diff's code has moved left, in whole cells.
fn text_offset(model: &Model) -> usize {
    review_ui::scroll::whole_rows(model.diff_text().offset())
}

/// Where a diff row is drawn.
#[derive(Debug, Clone, Copy)]
struct RowAt {
    /// The row's index among the diff's rows.
    index: usize,
    /// The screen row.
    y: u16,
    /// The list's area.
    area: Rect,
    /// How far code moves left, in cells.
    text_offset: usize,
}

/// Diff row `row` at `at`, on the list's `background`.
fn draw_row(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    row: &Row,
    at: RowAt,
    background: Rgba,
) {
    let frame = code::frame(model, at.index);
    match &row.body {
        Body::Code(cell) => code_row(painter, model, palette, row, cell, frame, at, background),
        Body::Pair(pair) => pair_row(painter, model, palette, row, pair, frame, at, background),
        Body::Note(note) => {
            let shown = annotation::annotation(model, palette, row, note, frame);
            let pan = annotation::note_pan(model, f32::from(reach(&shown)));
            annotation_row(painter, palette, &shown, frame, at, pan, background);
        }
    }
}

/// The bar along a hunk's rows at the row's left edge: gray, or the accent in the selected
/// hunk; nothing between hunks.
fn frame_bar(painter: &mut Painter<'_>, palette: &Palette, frame: Frame, x: u16, y: u16, bg: Rgba) {
    if let Some(color) = frame.bar(palette) {
        painter.symbol(x, y, "▎", Brush::new(color, bg));
    }
}

/// A line of code across the width: the frame bar, the gutter, the sign and the code, on the
/// line's background, or the hunk's band.
#[expect(
    clippy::too_many_arguments,
    reason = "a row's model, colors, data, frame and place, each needed and none a concept of its own"
)]
fn code_row(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    row: &Row,
    cell: &Cell,
    frame: Frame,
    at: RowAt,
    background: Rgba,
) {
    let parts = code::code_cell(
        model,
        palette,
        At {
            row: at.index,
            file: row.file,
            column: Column::Whole,
        },
        cell,
    );
    // The line's own background replaces the band, as one element's background replaces another.
    let bg = parts
        .background
        .or_else(|| frame.band(palette))
        .map_or(background, |color| over(background, color));
    let area = at.area;
    painter.fill(Rect::new(area.x, at.y, area.width, 1), bg);
    frame_bar(painter, palette, frame, area.x, at.y, bg);
    let lead = area.x + FRAME;
    code_cell(
        painter,
        palette,
        cell,
        &parts,
        CellAt {
            x: lead,
            y: at.y,
            width: area.right().saturating_sub(lead),
            skip: at.text_offset,
            bg,
        },
    );
}

/// Where a line of code's gutter, sign and code go.
#[derive(Debug, Clone, Copy)]
struct CellAt {
    /// The gutter's first cell.
    x: u16,
    y: u16,
    /// The cells from the gutter to the end of the code's room.
    width: u16,
    /// How far the code has moved left.
    skip: usize,
    /// The line's opaque background.
    bg: Rgba,
}

/// A line of code's gutter, sign and code at `at`.
fn code_cell(
    painter: &mut Painter<'_>,
    palette: &Palette,
    cell: &Cell,
    parts: &CodeCell,
    at: CellAt,
) {
    let CellAt {
        x,
        y,
        width: room,
        skip,
        bg,
    } = at;
    let right = x.saturating_add(room);
    // The number ends a cell before the gutter's edge, where the change bar goes; a number too
    // long for that takes the bar's cell too.
    let number = &parts.number;
    let digits = width(&number.text);
    let number_end = if digits < GUTTER {
        x + GUTTER - 1
    } else {
        x + GUTTER
    };
    let number_at = number_end.saturating_sub(digits).max(x);
    painter.piece(
        number_at,
        y,
        number_end.min(right).saturating_sub(number_at),
        number,
        bg,
    );
    if let Some(bar) = parts.bar
        && digits < GUTTER
    {
        painter.symbol(x + GUTTER - 1, y, "▕", Brush::new(bar, bg));
    }
    let sign_at = x + GUTTER;
    if sign_at < right {
        painter.piece(sign_at, y, 1, &parts.sign, bg);
    }
    let text_at = x + GUTTER + SIGN;
    let text_room = right.saturating_sub(text_at);
    if text_room == 0 {
        return;
    }
    let ink = Ink {
        foreground: parts.text_color.unwrap_or(palette.text),
        background: bg,
    };
    let taken = painter.layered(text_at, y, text_room, skip, &cell.text, &parts.layers, ink);
    let cells = DisplayLine::new(&cell.text).width();
    if let Some(blank) = &parts.blank {
        let blank_at = text_at + taken;
        if blank_at < right && cells >= skip {
            painter.piece(blank_at, y, right - blank_at, blank, bg);
        }
    }
    clip_mark(painter, palette, cells, skip, text_at, text_room, y, bg);
}

/// The clip marker over the last cell of a line of code `cells` wide, moved `skip` cells left
/// and shown in the `room` cells from `x` on row `y`, when the line runs past them.
#[expect(
    clippy::too_many_arguments,
    reason = "the line's width and pan, where its room is, and its background, none a concept of its own"
)]
pub fn clip_mark(
    painter: &mut Painter<'_>,
    palette: &Palette,
    cells: usize,
    skip: usize,
    x: u16,
    room: u16,
    y: u16,
    bg: Rgba,
) {
    let pan = review_ui::scroll::rows(skip);
    if room > 0 && code::clipped(cells, pan, f64::from(room)) {
        painter.piece(x + room - 1, y, 1, &code::clip_marker(palette), bg);
    }
}

/// A side-by-side row: the frame bar, the base half, the divider and the head half.
#[expect(
    clippy::too_many_arguments,
    reason = "a row's model, colors, data, frame and place, each needed and none a concept of its own"
)]
fn pair_row(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    row: &Row,
    pair: &Pair,
    frame: Frame,
    at: RowAt,
    background: Rgba,
) {
    let area = at.area;
    let row_bg = frame
        .band(palette)
        .map_or(background, |band| over(background, band));
    painter.fill(Rect::new(area.x, at.y, area.width, 1), row_bg);
    frame_bar(painter, palette, frame, area.x, at.y, row_bg);
    let (left, right) = halves(area.width);
    let left_at = area.x + FRAME;
    let divider_at = left_at + left;
    let right_at = divider_at + DIVIDER;
    for (cell, column, x, cells) in [
        (pair.left.as_ref(), Column::Left, left_at, left),
        (pair.right.as_ref(), Column::Right, right_at, right),
    ] {
        let Some(cell) = cell else {
            // A side without a line is hatched.
            let hatch = "╱".repeat(usize::from(cells));
            painter.text(x, at.y, cells, &hatch, Brush::new(palette.hairline, row_bg));
            continue;
        };
        let parts = code::code_cell(
            model,
            palette,
            At {
                row: at.index,
                file: row.file,
                column,
            },
            cell,
        );
        // A half's background lies over the row's band.
        let bg = parts.background.map_or(row_bg, |color| over(row_bg, color));
        painter.fill(Rect::new(x, at.y, cells, 1), bg);
        code_cell(
            painter,
            palette,
            cell,
            &parts,
            CellAt {
                x,
                y: at.y,
                width: cells,
                skip: at.text_offset,
                bg,
            },
        );
    }
    if divider_at < area.right() {
        painter.symbol(divider_at, at.y, "│", Brush::new(palette.hairline, row_bg));
    }
}

/// One of the tool's own rows: a hunk header, a gap, a note, a move or a description.
///
/// Its text moves with the code as `pan` says, only as far as it runs past its room, so a row
/// that fits never moves and where a hunk is and where a move goes stay in view; the clip
/// marker covers the last cell while some of the text is still cut.
fn annotation_row(
    painter: &mut Painter<'_>,
    palette: &Palette,
    shown: &Annotation,
    frame: Frame,
    at: RowAt,
    pan: NotePan,
    background: Rgba,
) {
    let area = at.area;
    let y = at.y;
    let row = Rect::new(area.x, y, area.width, 1);
    let right = area.right();
    let code_at = area.x + FRAME + GUTTER + SIGN;
    match shown {
        // Air between hunks: nothing, not even the frame bar.
        Annotation::Spacer => {}
        Annotation::Gap { label, rule } => {
            frame_bar(painter, palette, frame, area.x, y, background);
            let brush = Brush::new(*rule, background);
            let lead = "─".repeat(usize::from(GAP_LEAD));
            let mut x = code_at;
            x += painter.text(x, y, right.saturating_sub(x), &lead, brush);
            x += 1;
            x += painter.piece(x, y, right.saturating_sub(x), label, background);
            x += 1;
            if x < right {
                let rest = "─".repeat(usize::from(right - x));
                painter.text(x, y, right - x, &rest, brush);
            }
        }
        Annotation::Header(header) => {
            hunk_header(painter, palette, header, frame, row, pan, background);
        }
        Annotation::Note {
            text,
            tag,
            accent,
            background: band,
        } => {
            let bg = over(background, *band);
            painter.fill(row, bg);
            frame_bar(painter, palette, frame, area.x, y, bg);
            painter.symbol(area.x + FRAME + GUTTER, y, "▎", Brush::new(*accent, bg));
            let text_at = NoteAt {
                x: code_at,
                y,
                room: right.saturating_sub(code_at),
                pan,
                bg,
            };
            note_text(painter, palette, text_at, |painter, right| {
                let x = painter.piece(0, 0, right, text, bg);
                if let Some(tag) = tag {
                    painter.piece(x + 1, 0, right.saturating_sub(x + 1), tag, bg);
                }
            });
        }
        Annotation::Line {
            text,
            hint,
            background: band,
        } => {
            let bg = over(background, *band);
            painter.fill(row, bg);
            frame_bar(painter, palette, frame, area.x, y, bg);
            let text_at = NoteAt {
                x: code_at,
                y,
                room: right.saturating_sub(code_at),
                pan,
                bg,
            };
            note_text(painter, palette, text_at, |painter, right| {
                let x = painter.piece(0, 0, right, text, bg);
                if let Some(hint) = hint {
                    painter.piece(x + 2, 0, right.saturating_sub(x + 2), hint, bg);
                }
            });
        }
    }
}

/// The cells one of the tool's own rows draws from where code starts, unmoved, which is what
/// moves it with the code; a hunk header's outer scopes make way first, so they count for
/// nothing, and the cell it keeps free at the right edge counts too.
fn reach(shown: &Annotation) -> u16 {
    match shown {
        Annotation::Spacer | Annotation::Gap { .. } => 0,
        Annotation::Header(header) => header_reach(header),
        Annotation::Note { text, tag, .. } => width(&text.text).saturating_add(
            tag.as_ref()
                .map_or(0, |tag| width(&tag.text).saturating_add(1)),
        ),
        Annotation::Line { text, hint, .. } => width(&text.text).saturating_add(
            hint.as_ref()
                .map_or(0, |hint| width(&hint.text).saturating_add(2)),
        ),
    }
}

/// The cells [`hunk_header`] draws of `header` from where code starts, unmoved, as [`reach`]
/// counts them.
fn header_reach(header: &HunkHeader) -> u16 {
    let inner = header
        .scope
        .as_ref()
        .map_or(0, |scope| width(&scope.inner.text));
    let location = header.location.as_ref().map_or(0, |piece| {
        let before = if header.scope.is_some() {
            TAG_GAP_CELLS
        } else {
            0
        };
        width(&piece.text).saturating_add(before)
    });
    let tag = header.tag.as_ref().map_or(0, |tag| {
        let first = header.scope.is_none() && header.location.is_none();
        let before = if first { 0 } else { TAG_GAP_CELLS };
        width(&tag.text).saturating_add(before)
    });
    inner
        .saturating_add(location)
        .saturating_add(tag)
        .saturating_add(1)
}

/// Where the text of one of the tool's own rows goes, and how it moves with the code.
#[derive(Debug, Clone, Copy)]
struct NoteAt {
    /// Its first cell.
    x: u16,
    y: u16,
    /// The cells it has.
    room: u16,
    /// How far it has moved left, and whether some of it is still cut.
    pan: NotePan,
    /// The row's opaque background.
    bg: Rgba,
}

/// Draws the text `draw` draws (from the first cell of the room it is given) at `at`, moved
/// left with the code as its pan says, with the clip marker over the last cell while some of
/// it is still cut.
fn note_text(
    painter: &mut Painter<'_>,
    palette: &Palette,
    at: NoteAt,
    draw: impl FnOnce(&mut Painter<'_>, u16),
) {
    let NoteAt {
        x,
        y,
        room,
        pan: NotePan { shift, cut },
        bg,
    } = at;
    let shift = u16::try_from(review_ui::scroll::whole_rows(shift)).unwrap_or(u16::MAX);
    painter.shifted(x, y, room, shift, bg, draw);
    if room > 0 && cut {
        painter.piece(x + room - 1, y, 1, &code::clip_marker(palette), bg);
    }
}

/// A hunk's header: its fold arrow in the gutter, its state glyph in the sign column, then its
/// scopes (the outer ones cut first when room runs out) and its lines, as text on the band or
/// as a chip, and its tag last, an italic aside on the band; it moves with the code as `pan`
/// says, as every one of the tool's own rows does.
fn hunk_header(
    painter: &mut Painter<'_>,
    palette: &Palette,
    header: &HunkHeader,
    frame: Frame,
    row: Rect,
    pan: NotePan,
    background: Rgba,
) {
    let HunkHeader {
        background: band,
        fold,
        glyph,
        scope,
        tag,
        location,
    } = header;
    let bg = over(background, *band);
    let y = row.y;
    painter.fill(row, bg);
    frame_bar(painter, palette, frame, row.x, y, bg);
    painter.piece(row.x + FRAME + GUTTER - 2, y, 1, fold, bg);
    painter.piece(row.x + FRAME + GUTTER, y, 1, glyph, bg);
    let x = row.x + FRAME + GUTTER + SIGN;
    let right = row.right().saturating_sub(1);
    // The parts after the scopes, each an aside with its gap before it.
    let location_width = location
        .as_ref()
        .map_or(0, |piece| width(&piece.text) + TAG_GAP_CELLS);
    let tag_width = tag
        .as_ref()
        .map_or(0, |tag| width(&tag.text) + TAG_GAP_CELLS);
    let after = location_width + tag_width;
    // The outer scopes make way first, so what the header needs is the rest.
    let inner_width = scope.as_ref().map_or(0, |scope| width(&scope.inner.text));
    let at = NoteAt {
        x,
        y,
        room: right.saturating_sub(x),
        pan,
        bg,
    };
    note_text(painter, palette, at, |painter, right| {
        let mut x = 0;
        if let Some(Scope { outer, inner }) = scope {
            let room = right.saturating_sub(x);
            if let Some(outer) = outer {
                let outer_room = room.saturating_sub(inner_width + after);
                if outer_room > 0 {
                    let shown = ellipsize_end(&outer.text, outer_room);
                    x += painter.text(x, 0, outer_room, &shown, Brush::piece(outer, bg));
                }
            }
            x += painter.piece(x, 0, right.saturating_sub(x), inner, bg);
        }
        // The location and the tag are asides on the band itself, so the band stays whole.
        if let Some(piece) = location {
            if x > 0 {
                x += TAG_GAP_CELLS;
            }
            if x >= right {
                return;
            }
            x += painter.piece(x, 0, right - x, piece, bg);
        }
        if let Some(tag) = tag {
            if x > 0 {
                x += TAG_GAP_CELLS;
            }
            if x >= right {
                return;
            }
            painter.piece(x, 0, right - x, tag, bg);
        }
    });
}

/// The selected file's header: its change kind, its path with the directory cut first, its
/// details, then its line counts and its progress meter at the right edge when they fit.
fn file_header(painter: &mut Painter<'_>, model: &Model, palette: &Palette, area: Rect) {
    if area.height == 0 {
        return;
    }
    let surface = opaque(palette.surface);
    painter.fill(area, surface);
    let budget = area.width.saturating_sub(2);
    let x = area.x + 1;
    let y = area.y;
    let Some(file) = header::file_header(
        model,
        palette,
        Room {
            budget: f32::from(budget),
            cell: 1.0,
            gap: f32::from(HEADER_GAP),
        },
        1.0,
    ) else {
        painter.text(
            x,
            y,
            budget,
            header::NO_FILE,
            Brush::new(palette.faint, surface),
        );
        return;
    };
    let FileHeader {
        kind,
        dir,
        name,
        details,
        lines,
        counts,
        meter,
    } = file;
    let right = x + budget;
    // The right edge: the file's length, the counts, then the meter.
    let meter_width = meter
        .as_ref()
        .map_or(0, |meter| 5 + 1 + width(&meter.counts.text));
    let counts_width = counts.as_ref().map_or(0, |[added, removed]| {
        width(&added.text) + HEADER_GAP + width(&removed.text)
    });
    let lines_width = lines.as_ref().map_or(0, |lines| width(&lines.text));
    let parts = [lines_width, counts_width, meter_width];
    let shown = u16::try_from(parts.iter().filter(|&&part| part > 0).count()).unwrap_or(0);
    let tail = parts.iter().sum::<u16>() + shown.saturating_sub(1) * HEADER_GAP;
    let tail_at = right.saturating_sub(tail);
    let mut at = tail_at;
    if let Some(lines) = &lines {
        at += painter.piece(at, y, right - at, lines, surface) + HEADER_GAP;
    }
    if let Some([added, removed]) = &counts {
        at += painter.piece(at, y, right - at, added, surface) + HEADER_GAP;
        at += painter.piece(at, y, right.saturating_sub(at), removed, surface) + HEADER_GAP;
    }
    if let Some(meter) = &meter {
        draw_meter(painter, meter, at, y, right, surface);
    }
    // The left: the kind, the path and the details in what is left.
    let left_end = if tail > 0 {
        tail_at.saturating_sub(HEADER_GAP)
    } else {
        right
    };
    let mut at = x;
    if let Some(kind) = &kind {
        at += painter.piece(at, y, left_end.saturating_sub(at), kind, surface) + HEADER_GAP;
    }
    let room = left_end.saturating_sub(at);
    let name_width = width(&name.text).min(room);
    let dir_room = room - name_width;
    if dir_room > 0 && !dir.text.is_empty() {
        let shown = ellipsize_start(&dir.text, dir_room);
        at += painter.text(at, y, dir_room, &shown, Brush::piece(&dir, surface));
    }
    let shown = ellipsize_end(&name.text, name_width);
    at += painter.text(at, y, name_width, &shown, Brush::piece(&name, surface));
    let details_at = at + HEADER_GAP;
    if !details.text.is_empty() && details_at < left_end {
        let shown = ellipsize_end(&details.text, left_end - details_at);
        painter.text(
            details_at,
            y,
            left_end - details_at,
            &shown,
            Brush::piece(&details, surface),
        );
    }
}

/// A reviewed/total meter from `x`: its five-cell bar, then its counts.
fn draw_meter(painter: &mut Painter<'_>, meter: &Meter, x: u16, y: u16, right: u16, bg: Rgba) {
    let Meter { bar, counts } = meter;
    let cells = 5.min(right.saturating_sub(x));
    meter_bar(painter, x, y, cells, bar, bg);
    let at = x + cells + 1;
    painter.piece(at, y, right.saturating_sub(at), counts, bg);
}

/// A card in the middle of `area`: what is going on and more about it, wrapped to the card.
pub fn state_card(painter: &mut Painter<'_>, palette: &Palette, area: Rect, card: &StateCard) {
    let surface = opaque(palette.surface);
    let inner = 64.min(area.width.saturating_sub(8)).max(1);
    let detail = &card.detail.text;
    let lines: Vec<&str> = wrap(detail, inner)
        .into_iter()
        .map(|range| detail.get(range).unwrap_or_default())
        .collect();
    let widest = lines
        .iter()
        .map(|line| width(line))
        .chain([width(&card.title.text)])
        .max()
        .unwrap_or(0)
        .min(inner);
    let card_width = widest + 6;
    let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let card_height = rows.saturating_add(5);
    let x = area.x + area.width.saturating_sub(card_width) / 2;
    let y = area.y + area.height.saturating_sub(card_height) / 2;
    let card_area = Rect::new(x, y, card_width, card_height).intersection(area);
    painter.fill(card_area, surface);
    border(painter, palette.hairline, card_area, surface);
    painter.piece(x + 3, y + 2, widest, &card.title, surface);
    for (line, row) in lines.iter().zip(y + 3..) {
        if row + 1 >= card_area.bottom() {
            break;
        }
        painter.text(
            x + 3,
            row,
            widest,
            line,
            Brush::piece(&card.detail, surface),
        );
    }
}

/// A box around `area` in `color`.
pub fn border(painter: &mut Painter<'_>, color: Hsla, area: Rect, bg: Rgba) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let brush = Brush::new(color, bg);
    let (left, right) = (area.left(), area.right() - 1);
    let (top, bottom) = (area.top(), area.bottom() - 1);
    for x in left + 1..right {
        painter.symbol(x, top, "─", brush);
        painter.symbol(x, bottom, "─", brush);
    }
    for y in top + 1..bottom {
        painter.symbol(left, y, "│", brush);
        painter.symbol(right, y, "│", brush);
    }
    painter.symbol(left, top, "┌", brush);
    painter.symbol(right, top, "┐", brush);
    painter.symbol(left, bottom, "└", brush);
    painter.symbol(right, bottom, "┘", brush);
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use review_ui::config::Background;
    use review_ui::paint::Piece;

    use super::*;
    use crate::color::Colors;

    /// The symbols of the first row of `buffer`.
    fn row(buffer: &Buffer) -> String {
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, 0)).map_or("", |cell| cell.symbol()))
            .collect()
    }

    /// One of the tool's own rows moves and shows its clip marker exactly as the shared pan
    /// says for the cells it draws: one that just fits never moves or shows the marker however
    /// far the code pans, and one that is cut moves exactly as far as it runs past.
    #[test]
    fn notes_move_and_mark_as_the_shared_pan_says() {
        let (themes, _) = review_ui::theme::all(None);
        let Some(theme) = themes.first() else {
            panic!("built-in themes");
        };
        let palette = Palette::new(theme, None, None, Background::Bold);
        let bg = opaque(palette.band);
        let room = 10;
        let code_at = usize::from(FRAME + GUTTER + SIGN);
        // The row's cells from where code starts, the code having moved `offset` cells left.
        let draw = |shown: &Annotation, offset: f64| {
            let area = Rect::new(0, 0, FRAME + GUTTER + SIGN + room, 1);
            // How far it runs past its room, as the shared geometry gives it for a terminal.
            let overflow = (f64::from(reach(shown)) - f64::from(room)).max(0.0);
            let mut buffer = Buffer::empty(area);
            let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
            let at = RowAt {
                index: 0,
                y: 0,
                area,
                text_offset: 0,
            };
            let pan = NotePan::new(offset, overflow);
            annotation_row(&mut painter, &palette, shown, Frame::Outside, at, pan, bg);
            row(&buffer).chars().skip(code_at).collect::<String>()
        };
        let note = |text: &str, tag: &str| Annotation::Note {
            text: Piece::new(text, palette.text),
            tag: Some(Piece::new(tag, palette.faint)),
            accent: palette.accent,
            background: palette.band,
        };

        // A note that just fits, which its upper bound would count three cells wider.
        let fits = note("abcdef", "xyz");
        assert_eq!(reach(&fits), room);
        assert_eq!(draw(&fits, 0.0), "abcdef xyz");
        assert_eq!(draw(&fits, 3.0), "abcdef xyz");

        // A note two cells too wide: cut and marked, moved part of the way, and moved exactly
        // as far as it runs past however far the code goes.
        let cut = note("abcdefgh", "xyz");
        assert_eq!(draw(&cut, 0.0), "abcdefgh …");
        assert_eq!(draw(&cut, 1.0), "bcdefgh x…");
        assert_eq!(draw(&cut, 5.0), "cdefgh xyz");

        // A hunk header that just fits, the last cell kept free.
        let header = Annotation::Header(HunkHeader {
            background: palette.band,
            fold: Piece::new("▾", palette.faint),
            glyph: Piece::new("○", palette.faint),
            scope: Some(Scope {
                outer: None,
                inner: Piece::new("fn", palette.text),
            }),
            location: None,
            tag: Some(Piece::new("tag", palette.faint)),
        });
        assert_eq!(reach(&header), room);
        assert_eq!(draw(&header, 3.0), "fn    tag ", "the tag stands apart");
    }
}
