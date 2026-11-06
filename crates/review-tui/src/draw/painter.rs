//! Writing text into the terminal's cells: every text goes through review-ui's
//! [`DisplayLine`], so nothing from the review (paths, code, titles) ever reaches the terminal
//! as control characters, and every cell gets an opaque foreground and background.
//!
//! Cells are drawn in 24-bit colors, so a translucent layer drawn later, such as an overlay's
//! backdrop, composites over what is under it; [`Painter::finish`] then gives a terminal
//! without 24-bit color its nearest 256 colors.

use std::ops::Range;

use ratatui::buffer::{Buffer, CellWidth as _};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use review_ui::color::{Hsla, Rgba, rgb};
use review_ui::paint::display::DisplayLine;
use review_ui::paint::{Flat, Ink, Layer, Piece, Run, Styled, Weight, flatten};

use crate::color::{Colors, over};

/// How a cell is drawn: opaque colors and the text's style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Brush {
    /// The text color.
    pub fg: Rgba,
    /// The background.
    pub bg: Rgba,
    /// Whether the text is bold.
    pub bold: bool,
    /// Whether the text is italic.
    pub italic: bool,
    /// The underline's color, when the text is underlined.
    pub underline: Option<Rgba>,
    /// Whether the text is struck through.
    pub crossed: bool,
}

impl Brush {
    /// Text in `fg`, composited over the opaque `bg`.
    #[must_use]
    pub fn new(fg: Hsla, bg: Rgba) -> Self {
        Self {
            fg: over(bg, fg),
            bg,
            bold: false,
            italic: false,
            underline: None,
            crossed: false,
        }
    }

    /// `piece`'s color and weight over the opaque `bg`.
    #[must_use]
    pub fn piece(piece: &Piece, bg: Rgba) -> Self {
        Self {
            bold: piece.weight != Weight::Regular,
            ..Self::new(piece.color, bg)
        }
    }

    /// A stretch of a line flattened over its background.
    #[must_use]
    pub fn flat(flat: &Flat) -> Self {
        Self {
            fg: flat.foreground,
            bg: flat.background,
            bold: flat.bold,
            italic: flat.italic,
            underline: flat.underline,
            crossed: false,
        }
    }
}

/// Writes into a frame's cells in the terminal's colors.
pub struct Painter<'a> {
    buffer: &'a mut Buffer,
    colors: Colors,
}

impl<'a> Painter<'a> {
    /// A painter of `buffer` in `colors`.
    pub fn new(buffer: &'a mut Buffer, colors: Colors) -> Self {
        Self { buffer, colors }
    }

    /// The terminal's style of `brush`, in 24-bit colors until [`Self::finish`].
    fn style(brush: Brush) -> Style {
        let mut style = Style::default()
            .fg(true_color(brush.fg))
            .bg(true_color(brush.bg));
        let mut modifier = Modifier::empty();
        modifier.set(Modifier::BOLD, brush.bold);
        modifier.set(Modifier::ITALIC, brush.italic);
        modifier.set(Modifier::CROSSED_OUT, brush.crossed);
        if let Some(underline) = brush.underline {
            modifier |= Modifier::UNDERLINED;
            style = style.underline_color(true_color(underline));
        }
        style.add_modifier(modifier)
    }

    /// Lays the translucent `color` over every cell of `area`, its text and its background, as a
    /// window lays a backdrop over what is under it.
    pub fn veil(&mut self, area: Rect, color: Hsla) {
        let area = area.intersection(self.buffer.area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let Some(cell) = self.buffer.cell_mut((x, y)) else {
                    continue;
                };
                let veiled = |below: Color| match below {
                    Color::Rgb(red, green, blue) => {
                        let below = rgb(u32::from_be_bytes([0, red, green, blue]));
                        true_color(over(below, color))
                    }
                    other => other,
                };
                cell.fg = veiled(cell.fg);
                cell.bg = veiled(cell.bg);
                cell.underline_color = veiled(cell.underline_color);
            }
        }
    }

    /// Gives every cell the terminal's own color for its 24-bit one, once the frame is drawn.
    pub fn finish(self) {
        if self.colors == Colors::TrueColor {
            return;
        }
        let colors = self.colors;
        let terminal = |color: Color| match color {
            Color::Rgb(red, green, blue) => {
                colors.of(rgb(u32::from_be_bytes([0, red, green, blue])))
            }
            other => other,
        };
        for cell in &mut self.buffer.content {
            cell.fg = terminal(cell.fg);
            cell.bg = terminal(cell.bg);
            cell.underline_color = terminal(cell.underline_color);
        }
    }

    /// Fills `area` with blanks on `bg`.
    pub fn fill(&mut self, area: Rect, bg: Rgba) {
        let area = area.intersection(self.buffer.area);
        let style = Self::style(Brush {
            fg: bg,
            bg,
            bold: false,
            italic: false,
            underline: None,
            crossed: false,
        });
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = self.buffer.cell_mut((x, y)) {
                    cell.reset();
                    cell.set_symbol(" ").set_style(style);
                }
            }
        }
    }

    /// Writes one symbol, a cell wide, at `x`, `y`.
    pub fn symbol(&mut self, x: u16, y: u16, symbol: &str, brush: Brush) {
        let style = Self::style(brush);
        if let Some(cell) = self.buffer.cell_mut((x, y)) {
            cell.reset();
            cell.set_symbol(symbol).set_style(style);
        }
    }

    /// Writes `text` from `x` on row `y`, at most `width` cells of it, in `brush`; returns the
    /// cells it took.
    pub fn text(&mut self, x: u16, y: u16, width: u16, text: &str, brush: Brush) -> u16 {
        let line = DisplayLine::new(text);
        self.line(x, y, width, 0, &line, |_| brush)
    }

    /// Writes `piece` from `x` on row `y` over the opaque `bg`, at most `width` cells of it;
    /// returns the cells it took.
    pub fn piece(&mut self, x: u16, y: u16, width: u16, piece: &Piece, bg: Rgba) -> u16 {
        self.text(x, y, width, &piece.text, Brush::piece(piece, bg))
    }

    /// Writes the cells of `line` from its cell `skip` on, at `x` on row `y`, at most `width`
    /// of them, each glyph in the brush `brush` gives its first cell; returns the cells it took.
    ///
    /// A glyph cut by either edge shows as blanks, as a terminal cannot draw part of one.
    pub fn line(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        skip: usize,
        line: &DisplayLine,
        brush: impl Fn(usize) -> Brush,
    ) -> u16 {
        let end = skip + usize::from(width);
        for glyph in line.glyphs() {
            let start = glyph.cell;
            let stop = start + glyph.cells;
            if stop <= skip {
                continue;
            }
            if start >= end {
                break;
            }
            let style = Self::style(brush(start));
            let whole = start >= skip && stop <= end;
            for part in start.max(skip)..stop.min(end) {
                let Some(column) = column(x, part - skip) else {
                    continue;
                };
                let Some(target) = self.buffer.cell_mut((column, y)) else {
                    continue;
                };
                // A wide glyph's other cells are blanks it covers, as ratatui keeps them, and
                // ratatui's diff skips exactly the cells the glyph's text is wide.
                target.reset();
                if whole && part == start {
                    target.set_symbol(glyph.text);
                }
                target.set_style(style);
            }
        }
        // The cells taken, within the width.
        let taken = line.width().saturating_sub(skip).min(usize::from(width));
        u16::try_from(taken).unwrap_or(width)
    }

    /// Writes what `draw` draws, moved `skip` cells left: `draw` gets a scratch row `width +
    /// skip` cells wide, filled with `bg`, to draw in from its first cell as if that were its
    /// room, and the `width` cells from `skip` on land at `x` on row `y`.
    ///
    /// A wide glyph cut by either edge shows as blanks, as a terminal cannot draw part of one.
    pub fn shifted(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        skip: u16,
        bg: Rgba,
        draw: impl FnOnce(&mut Painter<'_>, u16),
    ) {
        let room = width.saturating_add(skip);
        let area = Rect::new(0, 0, room, 1);
        let mut scratch = Buffer::empty(area);
        let mut painter = Painter::new(&mut scratch, self.colors);
        painter.fill(area, bg);
        draw(&mut painter, room);
        for offset in 0..width {
            let (Some(from), Some(column)) =
                (scratch.cell((skip + offset, 0)), x.checked_add(offset))
            else {
                continue;
            };
            let Some(target) = self.buffer.cell_mut((column, y)) else {
                continue;
            };
            *target = from.clone();
            if from.cell_width() > width - offset {
                target.set_symbol(" ");
            }
        }
    }

    /// Writes the cells of `text` from its cell `skip` on, at `x` on row `y`, at most `width` of
    /// them, in `ink` with `layers` of runs over it, flattened as review-ui flattens them;
    /// returns the cells it took.
    #[expect(
        clippy::too_many_arguments,
        reason = "where the text goes, which part of it shows, and how it is inked, none a concept of its own"
    )]
    pub fn layered(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        skip: usize,
        text: &str,
        layers: &[Layer],
        ink: Ink,
    ) -> u16 {
        let line = DisplayLine::new(text);
        let plain = Brush {
            fg: over(ink.background, ink.foreground),
            bg: ink.background,
            bold: false,
            italic: false,
            underline: None,
            crossed: false,
        };
        // Each cell's brush, from the stretch of the line it shows.
        let mut brushes = vec![plain; line.width()];
        for flat in flatten(text.len(), layers, ink) {
            let brush = Brush::flat(&flat);
            for cell in line.cells(flat.range.clone()) {
                if let Some(slot) = brushes.get_mut(cell) {
                    *slot = brush;
                }
            }
        }
        self.line(x, y, width, skip, &line, |cell| {
            brushes.get(cell).copied().unwrap_or(plain)
        })
    }

    /// Writes `styled` from `x` on row `y`, at most `width` cells of it, in `color` with its runs
    /// over it, on the opaque `bg`, ending in `…` where it is cut; returns the cells it took.
    pub fn styled(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        styled: &Styled,
        color: Hsla,
        bg: Rgba,
    ) -> u16 {
        let cut = DisplayLine::new(&styled.text).width() > usize::from(width);
        let room = if cut { width.saturating_sub(1) } else { width };
        let ink = Ink {
            foreground: color,
            background: bg,
        };
        let taken = self.layered(
            x,
            y,
            room,
            0,
            &styled.text,
            std::slice::from_ref(&styled.runs),
            ink,
        );
        if cut && width > 0 {
            self.symbol(x + room, y, "…", Brush::new(color, bg));
            return taken + 1;
        }
        taken
    }
}

/// The 24-bit color of the opaque `color`.
fn true_color(color: Rgba) -> Color {
    let [red, green, blue] = color.to_bytes();
    Color::Rgb(red, green, blue)
}

/// The part `range` of `styled`, its runs cut to it; `range` lies on character boundaries.
#[must_use]
pub fn slice(styled: &Styled, range: Range<usize>) -> Styled {
    let runs = styled
        .runs
        .iter()
        .filter_map(|run| {
            let start = run.range.start.max(range.start);
            let end = run.range.end.min(range.end);
            (start < end).then(|| Run {
                range: start - range.start..end - range.start,
                style: run.style,
            })
        })
        .collect();
    Styled {
        text: styled.text.get(range).unwrap_or_default().to_owned(),
        runs,
    }
}

/// `text` wrapped at spaces into lines of at most `cells` cells, as the byte ranges of each
/// line; the spaces a line breaks at belong to no line, and a word wider than a line is cut
/// into pieces.
#[must_use]
pub fn wrap(text: &str, cells: u16) -> Vec<Range<usize>> {
    let cells = usize::from(cells.max(1));
    let mut lines = Vec::new();
    // The line being filled: where it starts, where its last word ends, and its width.
    let mut line: Option<(usize, usize, usize)> = None;
    let mut at = 0;
    for word in text.split(' ') {
        let mut start = at;
        let end = at + word.len();
        at = end + 1;
        let mut wide = DisplayLine::new(word).width();
        if let Some((line_start, line_end, filled)) = line {
            if filled + 1 + wide <= cells {
                line = Some((line_start, end, filled + 1 + wide));
                continue;
            }
            lines.push(line_start..line_end);
        }
        // A word wider than a line: whole lines of it, then the rest starts a line.
        while wide > cells {
            let cut = start + fitting(text.get(start..end).unwrap_or_default(), cells);
            lines.push(start..cut);
            start = cut;
            wide = DisplayLine::new(text.get(start..end).unwrap_or_default()).width();
        }
        line = Some((start, end, wide));
    }
    if let Some((start, end, _)) = line {
        lines.push(start..end);
    }
    if lines.is_empty() {
        lines.push(0..0);
    }
    lines
}

/// The bytes of the longest start of `text` that fits `cells`, at least its first glyph.
fn fitting(text: &str, cells: usize) -> usize {
    DisplayLine::new(text).fitting(cells)
}

/// The column `offset` cells right of `x`, when the terminal has one.
fn column(x: u16, offset: usize) -> Option<u16> {
    u16::try_from(offset)
        .ok()
        .and_then(|offset| x.checked_add(offset))
}

/// The cells `text` takes as the terminal shows it.
#[must_use]
pub fn width(text: &str) -> u16 {
    u16::try_from(DisplayLine::new(text).width()).unwrap_or(u16::MAX)
}

/// `text` cut to `cells` with `…` at its end when it is wider.
#[must_use]
pub fn ellipsize_end(text: &str, cells: u16) -> String {
    let shown = DisplayLine::new(text);
    if shown.width() <= usize::from(cells) {
        return shown.text().to_owned();
    }
    let room = usize::from(cells.saturating_sub(1));
    let mut kept: String = shown
        .glyphs()
        .take_while(|glyph| glyph.cell + glyph.cells <= room)
        .map(|glyph| glyph.text)
        .collect();
    if cells > 0 {
        kept.push('…');
    }
    kept
}

/// `text` cut to `cells` with `…` at its start when it is wider, as a path keeps its end.
#[must_use]
pub fn ellipsize_start(text: &str, cells: u16) -> String {
    let shown = DisplayLine::new(text);
    let width = shown.width();
    if width <= usize::from(cells) {
        return shown.text().to_owned();
    }
    let room = usize::from(cells.saturating_sub(1));
    let first = width - room;
    let mut kept = String::new();
    if cells > 0 {
        kept.push('…');
    }
    kept.extend(
        shown
            .glyphs()
            .skip_while(|glyph| glyph.cell < first)
            .map(|glyph| glyph.text),
    );
    kept
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;

    use super::*;

    fn white() -> Rgba {
        review_ui::color::rgb(0x00ff_ffff)
    }

    fn black() -> Rgba {
        review_ui::color::rgb(0)
    }

    /// The symbols of row `y` of `buffer`.
    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer.cell((x, y)).map_or("", |cell| cell.symbol()))
            .collect()
    }

    /// Control characters from the review show as visible placeholders and never reach the
    /// terminal as themselves.
    #[test]
    fn text_is_sanitized() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 1));
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        let brush = Brush::new(white().into(), black());
        painter.text(0, 0, 12, "a\u{1b}[31mb\tc", brush);
        assert_eq!(row(&buffer, 0), "a^[[31mb    ");
        assert!((0..12).all(|x| {
            buffer
                .cell((x, 0))
                .is_some_and(|cell| !cell.symbol().contains(char::is_control))
        }));
    }

    /// Wide characters take two cells; one cut by an edge shows blanks, and skipped cells
    /// pan the line.
    #[test]
    fn wide_characters_and_panning() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        let brush = Brush::new(white().into(), black());
        let line = DisplayLine::new("漢字ab");
        // The first cell of `漢` is skipped, so its second shows blank.
        assert_eq!(painter.line(0, 0, 4, 1, &line, |_| brush), 4);
        assert_eq!(row(&buffer, 0), " 字 a");
        // `字` would end past the third cell, so it shows blank.
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        assert_eq!(painter.line(0, 0, 3, 0, &line, |_| brush), 3);
        assert_eq!(row(&buffer, 0), "漢  a");
    }

    /// A cluster wider than its characters, such as a sign with an emoji presentation
    /// selector, takes the cells ratatui's diff skips for it, so the cell after it is drawn
    /// and a press on it lands on the next character.
    #[test]
    fn clusters_take_the_cells_ratatui_skips() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        let brush = Brush::new(white().into(), black());
        let line = DisplayLine::new("⚠\u{fe0f}x");
        assert_eq!(painter.line(0, 0, 4, 0, &line, |_| brush), 3);
        assert_eq!(row(&buffer, 0), "⚠\u{fe0f} x ");
        // A terminal that shows the previous frame gets the `x` in the third cell.
        let blank = Buffer::empty(Rect::new(0, 0, 4, 1));
        let updates: Vec<(u16, &str)> = blank
            .diff(&buffer)
            .into_iter()
            .map(|(x, _, cell)| (x, cell.symbol()))
            .collect();
        assert!(updates.contains(&(2, "x")), "{updates:?}");
        assert_eq!(line.byte_at(2), "⚠\u{fe0f}".len());
    }

    /// Every glyph is exactly as wide as ratatui counts its symbol, so ratatui's diff never
    /// skips a cell we drew or leaves one stale.
    #[test]
    fn glyph_widths_match_ratatui() {
        use ratatui::buffer::CellWidth as _;

        for text in [
            "plain\tcode",
            "漢字 😀 é ⚠\u{fe0f} ❤\u{fe0f}\u{200d}🔥 👩\u{200d}💻 🇺🇸 1\u{fe0f}\u{20e3}",
            "isAdmin\u{fe0f} 😀\u{fe0e} ⚠\u{fe0e} 1\u{fe0f} 漢\u{fe0f}",
            "\u{1b}[31m\u{9b}\u{7f}\u{202e}\u{200b}\u{3164}\u{e0041}",
            "\u{301}x\u{301}\u{302} ｶﾞ ﾊﾟ \u{1100}\u{1161}\u{11a8} \u{915}\u{94d}\u{937}",
        ] {
            for glyph in DisplayLine::new(text).glyphs() {
                assert_eq!(
                    usize::from(glyph.text.cell_width()),
                    glyph.cells,
                    "{:?} in {text:?}",
                    glyph.text
                );
            }
        }
    }

    /// A presentation selector outside an emoji, or one that would make an emoji narrower
    /// than terminals draw it, reaches the terminal as its code, never as itself.
    #[test]
    fn stray_selectors_reach_the_terminal_as_codes() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 19, 1));
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        let brush = Brush::new(white().into(), black());
        painter.text(0, 0, 19, "x\u{fe0f}😀\u{fe0e}", brush);
        assert_eq!(row(&buffer, 0), "x<U+FE0F>😀 <U+FE0E>");
        assert!((0..19).all(|x| {
            buffer
                .cell((x, 0))
                .is_some_and(|cell| !cell.symbol().contains(['\u{fe0e}', '\u{fe0f}']))
        }));
    }

    /// A wide character cut by the right edge shows blank, and the combining mark after it
    /// stays with it rather than joining the character before.
    #[test]
    fn a_cut_cluster_keeps_its_marks() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        let brush = Brush::new(white().into(), black());
        let line = DisplayLine::new("a漢\u{301}b");
        assert_eq!(painter.line(0, 0, 2, 0, &line, |_| brush), 2);
        assert_eq!(row(&buffer, 0), "a  ");
    }

    /// Text that does not fit is cut with an ellipsis, at the end or, for paths, at the start.
    #[test]
    fn ellipses() {
        assert_eq!(ellipsize_end("src/engine.rs", 8), "src/eng…");
        assert_eq!(ellipsize_end("short", 8), "short");
        assert_eq!(ellipsize_start("src/engine.rs", 8), "…gine.rs");
        assert_eq!(ellipsize_end("wide", 0), "");
    }

    /// The texts of `text`'s wrapped lines.
    fn wrapped(text: &str, cells: u16) -> Vec<&str> {
        wrap(text, cells)
            .into_iter()
            .map(|range| text.get(range).unwrap())
            .collect()
    }

    /// Text wraps at spaces, which belong to no line; a word wider than a line is cut into
    /// pieces, and an empty text is one empty line.
    #[test]
    fn text_wraps_at_spaces() {
        assert_eq!(
            wrapped("the quick brown fox", 9),
            ["the quick", "brown fox"]
        );
        assert_eq!(
            wrapped("a reconciliation", 6),
            ["a", "reconc", "iliati", "on"]
        );
        // A wide character never splits, so a line of its width takes it whole.
        assert_eq!(wrapped("漢字漢", 4), ["漢字", "漢"]);
        assert_eq!(wrapped("", 5), [""]);
    }

    /// A slice of styled text keeps the runs over it, shifted to its start.
    #[test]
    fn slices_keep_their_runs() {
        let bold = review_ui::paint::Style {
            bold: true,
            ..review_ui::paint::Style::default()
        };
        let styled = Styled {
            text: "let value = 1;".to_owned(),
            runs: vec![
                Run {
                    range: 0..3,
                    style: bold,
                },
                Run {
                    range: 4..9,
                    style: bold,
                },
            ],
        };
        let sliced = slice(&styled, 2..6);
        assert_eq!(sliced.text, "t va");
        assert_eq!(
            sliced.runs,
            [
                Run {
                    range: 0..1,
                    style: bold,
                },
                Run {
                    range: 2..4,
                    style: bold,
                },
            ]
        );
    }

    /// Styled text cut by its room ends in an ellipsis within the room, and fitting text does
    /// not.
    #[test]
    fn styled_text_ends_in_an_ellipsis_where_cut() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        let styled = |text: &str| Styled {
            text: text.to_owned(),
            runs: Vec::new(),
        };
        assert_eq!(
            painter.styled(0, 0, 6, &styled("src/engine.rs"), white().into(), black()),
            6
        );
        assert_eq!(row(&buffer, 0), "src/e…  ");
        let mut painter = Painter::new(&mut buffer, Colors::TrueColor);
        assert_eq!(
            painter.styled(0, 0, 6, &styled("lib.rs"), white().into(), black()),
            6
        );
        assert_eq!(row(&buffer, 0), "lib.rs  ");
    }

    /// A veil composites over the text and the background under it, as a backdrop dims a
    /// window, and a terminal without 24-bit color gets the nearest of its 256 once the frame
    /// is done.
    #[test]
    fn veils_composite_and_finish_maps_colors() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        let mut painter = Painter::new(&mut buffer, Colors::Indexed);
        painter.text(0, 0, 2, "ab", Brush::new(white().into(), white()));
        painter.veil(Rect::new(1, 0, 1, 1), Hsla::from(black()).alpha(0.5));
        let cell = |buffer: &Buffer, x: u16| buffer.cell((x, 0)).unwrap().clone();
        assert_eq!(cell(&buffer, 0).bg, Color::Rgb(255, 255, 255));
        let veiled = cell(&buffer, 1);
        assert!(
            matches!(veiled.bg, Color::Rgb(red, green, blue) if red == green && green == blue && (126..=129).contains(&red)),
            "{:?}",
            veiled.bg
        );
        assert_eq!(veiled.fg, veiled.bg);

        Painter::new(&mut buffer, Colors::Indexed).finish();
        assert_eq!(cell(&buffer, 0).bg, Color::Indexed(231));
        assert!(matches!(cell(&buffer, 1).bg, Color::Indexed(232..=255)));
    }
}
