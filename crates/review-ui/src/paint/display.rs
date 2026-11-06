//! Text as the review shows it on a grid of cells, in a terminal and in a window alike: what
//! each part of a line draws as, how many cells it takes, and where its bytes land.
//!
//! The rules, which every width the model computes follows:
//!
//! - A line draws in grapheme clusters (extended ones, as Unicode's UAX #29 defines them), each
//!   in the cells `unicode-width` gives the whole cluster, which is what ratatui's buffer
//!   assumes: `⚠️`, a sign and an emoji presentation selector, takes two cells, and `é` as `e`
//!   and a combining accent one.
//! - A tab is [`TAB_CELLS`] spaces.
//! - A C0 control character (U+0000 to U+001F, apart from the tab) and DEL (U+007F) show in
//!   caret notation, as vim and `cat -v` show them: `^@` to `^_`, and `^?`.
//! - A C1 control character (U+0080 to U+009F) shows as its code in angle brackets, as vim
//!   shows it: `<80>` to `<9f>`.
//! - A character Unicode lets a renderer draw as nothing shows as its code point, such as
//!   `<U+202E>`: the default ignorable code points (bidi controls, zero-width spaces and
//!   joiners, variation selectors, tag characters, Hangul fillers and the like), the line and
//!   paragraph separators, and the interlinear annotation characters.
//! - Two of those belong to the character before them and draw with it: a zero-width joiner
//!   inside a cluster followed by the pictograph it joins, as in `👩‍💻`, and an emoji
//!   presentation selector (U+FE0F) right after a pictograph, as in `⚠️`, or in a keycap, as
//!   in `1️⃣`.
//!   Any other joiner or emoji presentation selector shows as its code, between letters too,
//!   and so do the tags of a subdivision flag: each of them can hide a character in an
//!   identifier, such as `isAdmin` and `isAdmin` with a selector after it, while in an emoji
//!   they only pick how pictographs draw.
//! - The text presentation selector (U+FE0E) always shows as its code: all it can change is
//!   how wide an emoji draws, where `unicode-width` gives one cell and most terminals draw two,
//!   and anywhere else it draws as nothing.
//! - A cluster that would take no cells, such as a combining mark with nothing to combine
//!   with, shows as the code points of its characters.
//!
//! So every character of a line takes at least one cell, and none can move a terminal's cursor,
//! change its colors, reorder the text around it or vanish: each of these could hide code from
//! the reviewer or make it read as something it is not, as Trojan Source attacks do.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::Run;
use crate::rows::TAB_CELLS;

/// The zero-width joiner.
const JOINER: char = '\u{200d}';

/// The emoji presentation selector, VS16.
const EMOJI_SELECTOR: char = '\u{fe0f}';

/// The combining enclosing keycap, which ends a keycap sequence such as `1️⃣`.
const KEYCAP: char = '\u{20e3}';

/// How many monospace cells `text` takes as it shows, by the rules of the module
/// documentation: the widest line, not the longest in bytes, sets how far a view scrolls
/// sideways.
#[must_use]
pub fn display_width(text: &str) -> usize {
    if is_plain(text) {
        let tabs = text.bytes().filter(|&byte| byte == b'\t').count();
        return text.len() + tabs * (TAB_CELLS - 1);
    }
    let mut cells = 0;
    walk(text, |_, shape| cells += shape.cells());
    cells
}

/// Where one part of a line shows: a cluster, or the part of one that draws as itself, a
/// character shown by its code, or a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Part {
    /// Its first byte in the source line.
    byte: usize,
    /// Its first byte in the shown text.
    shown: usize,
    /// Its first cell.
    cell: usize,
    /// Whether each byte of its shown text, all ASCII, takes a cell of its own, as a
    /// placeholder's and a tab's do; otherwise its shown text is one cluster.
    spread: bool,
}

/// A line as it shows; see the module documentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayLine {
    /// The text to draw.
    text: String,
    /// Every part of the line, in order, then the end of the line.
    parts: Vec<Part>,
}

/// What one cell of a terminal draws: a cluster in the first of the cells it covers, or one
/// character of a placeholder or a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Glyph<'a> {
    /// The text to draw.
    pub text: &'a str,
    /// Its first cell.
    pub cell: usize,
    /// The cells it covers, at least one.
    pub cells: usize,
}

impl DisplayLine {
    /// `line` as it shows.
    #[must_use]
    pub fn new(line: &str) -> Self {
        let mut text = String::with_capacity(line.len());
        let mut parts = Vec::with_capacity(line.len() + 1);
        let mut cell = 0;
        walk(line, |byte, shape| {
            parts.push(Part {
                byte,
                shown: text.len(),
                cell,
                spread: !matches!(shape, Shape::Text { .. }),
            });
            shape.push_to(&mut text);
            cell += shape.cells();
        });
        parts.push(Part {
            byte: line.len(),
            shown: text.len(),
            cell,
            spread: false,
        });
        Self { text, parts }
    }

    /// The text to draw, one cell per narrow character.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cells the line takes.
    #[must_use]
    pub fn width(&self) -> usize {
        self.parts.last().map_or(0, |end| end.cell)
    }

    /// What each cell of the line draws, in order; the cells a wide cluster covers after its
    /// first draw nothing of their own.
    pub fn glyphs(&self) -> impl Iterator<Item = Glyph<'_>> {
        self.parts
            .iter()
            .zip(self.parts.iter().skip(1))
            .flat_map(move |(part, next)| {
                let (each, count) = if part.spread {
                    (1, next.shown - part.shown)
                } else {
                    (next.shown - part.shown, 1)
                };
                (0..count).map(move |index| {
                    let start = part.shown + index * each;
                    Glyph {
                        text: self.text.get(start..start + each).unwrap_or_default(),
                        cell: if part.spread {
                            part.cell + index
                        } else {
                            part.cell
                        },
                        cells: if part.spread {
                            1
                        } else {
                            next.cell - part.cell
                        },
                    }
                })
            })
    }

    /// The part holding byte `byte` of the source line: the last one starting at or before it,
    /// the line's end at or past it.
    fn part_at(&self, byte: usize) -> Option<&Part> {
        let after = self.parts.partition_point(|part| part.byte <= byte);
        after.checked_sub(1).and_then(|index| self.parts.get(index))
    }

    /// The first part starting at or past byte `byte`, the line's end past its last part.
    fn part_from(&self, byte: usize) -> Option<&Part> {
        let next = self.parts.partition_point(|part| part.byte < byte);
        self.parts.get(next).or_else(|| self.parts.last())
    }

    /// The first cell of the part holding byte `byte` of the source line; the line's width at
    /// or past its end.
    #[must_use]
    pub fn cell_of(&self, byte: usize) -> usize {
        self.part_at(byte).map_or(0, |part| part.cell)
    }

    /// The cells the source bytes `bytes` are drawn in, such as a find match's or a
    /// selection's: from the cell of the part holding the first byte to the end of the part
    /// holding the last.
    #[must_use]
    pub fn cells(&self, bytes: Range<usize>) -> Range<usize> {
        let start = self.cell_of(bytes.start);
        if bytes.is_empty() {
            return start..start;
        }
        let end = self.part_from(bytes.end).map_or(start, |part| part.cell);
        start..end.max(start)
    }

    /// The bytes of the shown text the source bytes `bytes` are drawn as, as [`Self::cells`]
    /// gives their cells, for styling them in a window.
    #[must_use]
    pub fn shown(&self, bytes: Range<usize>) -> Range<usize> {
        let start = self.part_at(bytes.start).map_or(0, |part| part.shown);
        if bytes.is_empty() {
            return start..start;
        }
        let end = self.part_from(bytes.end).map_or(start, |part| part.shown);
        start..end.max(start)
    }

    /// `runs` over the source line, sorted and not overlapping, moved onto the shown text: a
    /// run covers every part its bytes touch, and gives way to the run before it where both
    /// touch one part.
    #[must_use]
    pub fn shown_runs(&self, runs: impl IntoIterator<Item = Run>) -> Vec<Run> {
        let mut end = 0;
        runs.into_iter()
            .filter_map(|run| {
                let shown = self.shown(run.range);
                let start = shown.start.max(end);
                (start < shown.end).then(|| {
                    end = shown.end;
                    Run {
                        range: start..shown.end,
                        style: run.style,
                    }
                })
            })
            .collect()
    }

    /// The source byte of the part drawn in cell `cell`, for a mouse press; the line's length
    /// past its last cell.
    ///
    /// Every cell of a wide cluster or a placeholder gives the part's start.
    #[must_use]
    pub fn byte_at(&self, cell: usize) -> usize {
        let after = self.parts.partition_point(|part| part.cell <= cell);
        after
            .checked_sub(1)
            .and_then(|index| self.parts.get(index))
            .map_or(0, |part| part.byte)
    }

    /// The source byte of the part that byte `shown` of the shown text belongs to, for a
    /// window that maps the mouse to the text it laid out; the line's length at or past the
    /// shown text's end.
    #[must_use]
    pub fn byte_at_shown(&self, shown: usize) -> usize {
        let after = self.parts.partition_point(|part| part.shown <= shown);
        after
            .checked_sub(1)
            .and_then(|index| self.parts.get(index))
            .map_or(0, |part| part.byte)
    }

    /// The bytes of the longest start of the line that fits `cells` cells, at least its first
    /// part, so text wrapped or cut by it never splits a cluster or a placeholder.
    #[must_use]
    pub fn fitting(&self, cells: usize) -> usize {
        let first_past = self
            .parts
            .iter()
            .zip(self.parts.iter().skip(1))
            .position(|(_, next)| next.cell > cells);
        let cut = match first_past {
            Some(0) => 1,
            Some(index) => index,
            None => return self.parts.last().map_or(0, |end| end.byte),
        };
        self.parts.get(cut).map_or(0, |part| part.byte)
    }
}

/// Whether `line` is printable ASCII and tabs, as most code is, which needs no clusters.
fn is_plain(line: &str) -> bool {
    line.bytes()
        .all(|byte| byte == b'\t' || (0x20..0x7f).contains(&byte))
}

/// How one part of a line shows.
#[derive(Debug, Clone, Copy)]
enum Shape<'a> {
    /// Source text that draws as itself in `cells` cells: a cluster, or the part of one that
    /// shows.
    Text { text: &'a str, cells: usize },
    /// A tab.
    Tab,
    /// A character shown by its code.
    Code(char),
}

impl Shape<'_> {
    /// The cells it takes.
    fn cells(self) -> usize {
        match self {
            Self::Text { cells, .. } => cells,
            Self::Tab => TAB_CELLS,
            Self::Code(character) => code_cells(character),
        }
    }

    /// Appends what it draws to `text`.
    fn push_to(self, text: &mut String) {
        match self {
            Self::Text { text: shown, .. } => text.push_str(shown),
            Self::Tab => text.extend(std::iter::repeat_n(' ', TAB_CELLS)),
            Self::Code(character) => push_code(text, character),
        }
    }
}

/// Calls `each` with the first byte and the shape of every part of `line`, in order.
fn walk<'a>(line: &'a str, mut each: impl FnMut(usize, Shape<'a>)) {
    if is_plain(line) {
        for (byte, character) in line.char_indices() {
            let shape = match character {
                '\t' => Shape::Tab,
                _ => Shape::Text {
                    text: line.get(byte..=byte).unwrap_or_default(),
                    cells: 1,
                },
            };
            each(byte, shape);
        }
        return;
    }
    for (start, cluster) in line.grapheme_indices(true) {
        cluster_parts(start, cluster, &mut each);
    }
}

/// Calls `each` with the parts of `cluster`, which starts at byte `start` of its line: its
/// characters that show as themselves, in runs, and the others by their codes.
fn cluster_parts<'a>(start: usize, cluster: &'a str, each: &mut impl FnMut(usize, Shape<'a>)) {
    // Where the run of characters that show as themselves starts, and its last character.
    let mut run: Option<usize> = None;
    let mut last: Option<char> = None;
    let mut characters = cluster.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        let next = characters.peek().map(|&(_, next)| next);
        if shows_as_itself(character, last, next) {
            run.get_or_insert(offset);
            last = Some(character);
            continue;
        }
        if let Some(from) = run.take() {
            text_part(
                start + from,
                cluster.get(from..offset).unwrap_or_default(),
                each,
            );
        }
        last = None;
        let shape = match character {
            '\t' => Shape::Tab,
            other => Shape::Code(other),
        };
        each(start + offset, shape);
    }
    if let Some(from) = run {
        text_part(start + from, cluster.get(from..).unwrap_or_default(), each);
    }
}

/// Hands `each` the characters `text`, which show as themselves, at byte `start`: as one part
/// in the cells they take, or by their codes when they would take none.
fn text_part<'a>(start: usize, text: &'a str, each: &mut impl FnMut(usize, Shape<'a>)) {
    let cells = text_cells(text);
    if cells > 0 {
        each(start, Shape::Text { text, cells });
        return;
    }
    for (offset, character) in text.char_indices() {
        each(start + offset, Shape::Code(character));
    }
}

/// The cells of `text`, at most one cluster, as ratatui counts a cell's symbol:
/// `unicode-width`'s width, and a cell for each halfwidth katakana sound mark, which terminals
/// draw apart from the character before it.
fn text_cells(text: &str) -> usize {
    let marks = text
        .chars()
        .filter(|&character| matches!(character, '\u{ff9e}' | '\u{ff9f}'))
        .count();
    text.width() + marks
}

/// Whether `character` shows as itself, after `last`, the character before it in its cluster
/// when that one shows as itself, and before `next`, the one after it in its cluster.
fn shows_as_itself(character: char, last: Option<char>, next: Option<char>) -> bool {
    match character {
        JOINER => last.is_some() && next.is_some_and(is_pictograph),
        EMOJI_SELECTOR => last.is_some_and(|last| {
            is_pictograph(last) || (is_keycap_base(last) && next == Some(KEYCAP))
        }),
        other => !other.is_control() && !is_invisible(other),
    }
}

/// Whether `character` can start a keycap sequence: `#`, `*` and the ASCII digits.
fn is_keycap_base(character: char) -> bool {
    matches!(character, '#' | '*' | '0'..='9')
}

/// Whether `character` is one a renderer may draw as nothing: a default ignorable code point,
/// as Unicode's `DerivedCoreProperties.txt` lists them, a line or paragraph separator, or an
/// interlinear annotation character.
fn is_invisible(character: char) -> bool {
    matches!(
        character,
        '\u{ad}'
            | '\u{34f}'
            | '\u{61c}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{feff}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fffb}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{1d173}'..='\u{1d17a}'
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// Whether `character` is a pictograph a joiner may join into an emoji: roughly Unicode's
/// `Extended_Pictographic`, its symbol blocks taken whole.
fn is_pictograph(character: char) -> bool {
    matches!(
        character,
        '\u{a9}'
            | '\u{ae}'
            | '\u{203c}'
            | '\u{2049}'
            | '\u{2122}'
            | '\u{2139}'
            | '\u{2194}'..='\u{21aa}'
            | '\u{231a}'..='\u{23ff}'
            | '\u{24c2}'
            | '\u{25aa}'..='\u{27bf}'
            | '\u{2934}'..='\u{2935}'
            | '\u{2b05}'..='\u{2b55}'
            | '\u{3030}'
            | '\u{303d}'
            | '\u{3297}'
            | '\u{3299}'
            | '\u{1f000}'..='\u{1faff}'
            | '\u{1fc00}'..='\u{1fffd}'
    )
}

/// The cells `character` takes shown by its code, one per character of [`push_code`]'s text.
fn code_cells(character: char) -> usize {
    let code = u32::from(character);
    match code {
        0..0x20 | 0x7f => 2,
        0x80..0xa0 => 4,
        // `<U+`, four to six hex digits and `>`.
        _ => 4 + hex_digits(code),
    }
}

/// How many hex digits `<U+…>` writes `code` with: four at least.
fn hex_digits(code: u32) -> usize {
    usize::try_from(code.bit_width().div_ceil(4)).map_or(6, |digits| digits.max(4))
}

/// Appends `character`'s code to `text`: caret notation for a C0 control and DEL, `<9b>` for
/// a C1 control, and `<U+202E>` for anything else.
fn push_code(text: &mut String, character: char) {
    let code = u32::from(character);
    match code {
        0x7f => text.push_str("^?"),
        0..0x20 => {
            text.push('^');
            text.push(char::from_u32(code + 0x40).unwrap_or('?'));
        }
        0x80..0xa0 => {
            text.push('<');
            push_hex(text, code, 2);
            text.push('>');
        }
        _ => {
            text.push_str("<U+");
            push_hex(text, code, hex_digits(code));
            text.push('>');
        }
    }
}

/// Appends the last `digits` hex digits of `code` to `text`: lowercase for two, as vim writes a
/// C1 control, and uppercase for more, as Unicode writes a code point.
fn push_hex(text: &mut String, code: u32, digits: usize) {
    for place in (0..digits).rev() {
        let nibble = u32::try_from(place * 4)
            .ok()
            .and_then(|shift| code.checked_shr(shift))
            .map_or(0, |shifted| shifted & 0xf);
        let digit = char::from_digit(nibble, 16).unwrap_or('?');
        text.push(if digits > 2 {
            digit.to_ascii_uppercase()
        } else {
            digit
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The texts and cells of `line`'s glyphs.
    fn glyphs(line: &str) -> Vec<(String, usize)> {
        DisplayLine::new(line)
            .glyphs()
            .map(|glyph| (glyph.text.to_owned(), glyph.cells))
            .collect()
    }

    /// Tabs expand to four spaces and control characters to visible placeholders; everything
    /// else stays as it is.
    #[test]
    fn controls_and_tabs_become_visible_text() {
        assert_eq!(DisplayLine::new("a\tb").text(), "a    b");
        assert_eq!(DisplayLine::new("\u{1b}[31m").text(), "^[[31m");
        assert_eq!(DisplayLine::new("\0\u{7f}").text(), "^@^?");
        assert_eq!(DisplayLine::new("x\u{85}y\u{9b}").text(), "x<85>y<9b>");
        assert_eq!(DisplayLine::new("\r").text(), "^M");
        assert_eq!(DisplayLine::new("é 漢 😀").text(), "é 漢 😀");
    }

    /// Bidi controls, zero-width characters, tags and fillers show by their codes, so an
    /// override cannot reorder code and an invisible identifier cannot hide in a call.
    #[test]
    fn invisible_characters_show_their_codes() {
        assert_eq!(
            DisplayLine::new("a\u{202e}b\u{2066}c").text(),
            "a<U+202E>b<U+2066>c"
        );
        assert_eq!(DisplayLine::new("exec(\u{3164})").text(), "exec(<U+3164>)");
        assert_eq!(
            DisplayLine::new("x\u{200b}\u{feff}\u{2060}y").text(),
            "x<U+200B><U+FEFF><U+2060>y"
        );
        assert_eq!(
            DisplayLine::new("\u{200e}\u{200f}\u{61c}\u{115f}\u{1160}\u{ffa0}").text(),
            "<U+200E><U+200F><U+061C><U+115F><U+1160><U+FFA0>"
        );
        assert_eq!(
            DisplayLine::new("id\u{e0041}\u{e007f}").text(),
            "id<U+E0041><U+E007F>"
        );
        assert_eq!(DisplayLine::new("a\u{2028}b").text(), "a<U+2028>b");
        // A zero-width joiner between letters hides nothing either.
        assert_eq!(DisplayLine::new("a\u{200d}b").text(), "a<U+200D>b");
        // A combining mark with nothing before it to combine with shows its code.
        assert_eq!(DisplayLine::new("\u{301}a").text(), "<U+0301>a");
    }

    /// An emoji's own joiners and presentation selectors draw with it; a selector after
    /// another one, or with nothing before it, shows its code, so a run of them cannot carry
    /// hidden data.
    #[test]
    fn emoji_keep_their_joiners_and_selectors() {
        assert_eq!(
            glyphs("⚠\u{fe0f}x"),
            [("⚠\u{fe0f}".to_owned(), 2), ("x".to_owned(), 1)]
        );
        assert_eq!(glyphs("👩\u{200d}💻"), [("👩\u{200d}💻".to_owned(), 2)]);
        // Ordinary emoji draw exactly as they are: ZWJ sequences with selectors, a flag and a
        // keycap.
        for emoji in [
            "👁\u{fe0f}\u{200d}🗨\u{fe0f}",
            "❤\u{fe0f}\u{200d}🔥",
            "🏳\u{fe0f}\u{200d}🌈",
            "🇺🇸",
            "1\u{fe0f}\u{20e3}",
            "#\u{fe0f}\u{20e3}",
            "©\u{fe0f}",
        ] {
            assert_eq!(DisplayLine::new(emoji).text(), emoji, "{emoji:?}");
        }
        assert_eq!(
            DisplayLine::new("⚠\u{fe0f}\u{fe0f}").text(),
            "⚠\u{fe0f}<U+FE0F>"
        );
        assert_eq!(DisplayLine::new("\u{fe0f}").text(), "<U+FE0F>");
        // An ideographic variation selector, another way to hide data, shows its code.
        assert_eq!(DisplayLine::new("a\u{e0100}").text(), "a<U+E0100>");
        // A trailing joiner joins nothing.
        assert_eq!(DisplayLine::new("👩\u{200d}").text(), "👩<U+200D>");
    }

    /// An emoji presentation selector after anything but a pictograph or a keycap's base
    /// shows its code, so `isAdmin` and `isAdmin` with a selector after it, two identifiers in
    /// JavaScript and Python, never look alike.
    #[test]
    fn selectors_outside_emoji_show_their_codes() {
        assert_eq!(
            DisplayLine::new("isAdmin\u{fe0f} = true").text(),
            "isAdmin<U+FE0F> = true"
        );
        assert_eq!(DisplayLine::new("é\u{fe0f}").text(), "é<U+FE0F>");
        assert_eq!(DisplayLine::new("漢\u{fe0f}").text(), "漢<U+FE0F>");
        // A digit takes a selector only in a keycap, with the keycap after it.
        assert_eq!(DisplayLine::new("x1\u{fe0f}").text(), "x1<U+FE0F>");
        // The keycap, with nothing left to enclose, shows its code too.
        assert_eq!(
            DisplayLine::new("a\u{fe0f}\u{20e3}").text(),
            "a<U+FE0F><U+20E3>"
        );
    }

    /// A text presentation selector always shows its code: after an emoji it would make the
    /// drawn width disagree with a terminal's, and anywhere else it hides a code point.
    #[test]
    fn text_selectors_show_their_codes() {
        let line = DisplayLine::new("😀\u{fe0e}x");
        assert_eq!(line.text(), "😀<U+FE0E>x");
        assert_eq!(line.width(), 2 + 8 + 1);
        assert_eq!(DisplayLine::new("⚠\u{fe0e}").text(), "⚠<U+FE0E>");
        assert_eq!(DisplayLine::new("a\u{fe0e}").text(), "a<U+FE0E>");
        assert_eq!(display_width("😀\u{fe0e}"), 10);
    }

    /// Wide characters take two cells, combining marks none, tabs four and placeholders as
    /// many as their text; a cluster takes the cells of the whole, as ratatui counts it.
    #[test]
    fn widths_follow_what_shows() {
        assert_eq!(DisplayLine::new("").width(), 0);
        assert_eq!(DisplayLine::new("abc").width(), 3);
        assert_eq!(DisplayLine::new("漢字").width(), 4);
        assert_eq!(DisplayLine::new("😀!").width(), 3);
        assert_eq!(DisplayLine::new("e\u{301}").width(), 1);
        assert_eq!(DisplayLine::new("\ta").width(), 5);
        assert_eq!(DisplayLine::new("\u{1b}\u{9b}").width(), 6);
        assert_eq!(DisplayLine::new("⚠\u{fe0f}x").width(), 3);
        assert_eq!(DisplayLine::new("\u{202e}").width(), 8);
        assert_eq!(DisplayLine::new("\u{e0041}").width(), 9);
    }

    /// The model's widths are exactly the cells a line draws in, whatever it holds, so panning
    /// reaches the end of every line and a cursor lands where it draws.
    #[test]
    fn display_width_is_the_drawn_width() {
        for line in [
            "plain code();",
            "\tindented",
            "a\u{1b}[31mb\u{9b}",
            "x\u{202e}\u{200b}y",
            "⚠\u{fe0f}x 👩\u{200d}💻 é 漢",
            "\u{301}\u{3164}\u{e0041}",
            "ｶﾞ\r",
        ] {
            assert_eq!(
                display_width(line),
                DisplayLine::new(line).width(),
                "{line:?}"
            );
        }
    }

    /// Byte offsets land on the cells their characters are drawn in, so a match or a selection
    /// covers exactly its characters.
    #[test]
    fn bytes_map_to_their_cells() {
        // `a` at cell 0, the tab at 1..5, `漢` (three bytes) at 5..7, `b` at 7.
        let line = DisplayLine::new("a\t漢b");
        assert_eq!(line.cell_of(0), 0);
        assert_eq!(line.cell_of(1), 1);
        assert_eq!(line.cell_of(2), 5);
        assert_eq!(line.cell_of(3), 5);
        assert_eq!(line.cell_of(5), 7);
        assert_eq!(line.cell_of(6), 8);
        assert_eq!(line.cell_of(60), 8);
        assert_eq!(line.cells(2..5), 5..7);
        assert_eq!(line.cells(1..6), 1..8);
        assert_eq!(line.cells(3..3), 5..5);
        // An emoji is four bytes and two cells; the escape before it is two cells.
        let line = DisplayLine::new("\u{1b}😀x");
        assert_eq!(line.cells(0..1), 0..2);
        assert_eq!(line.cells(1..5), 2..4);
        assert_eq!(line.cells(5..6), 4..5);
        // A sign and its selector are one cluster, so either byte lands on both its cells.
        let line = DisplayLine::new("⚠\u{fe0f}x");
        assert_eq!(line.cells(3..6), 0..2);
        assert_eq!(line.cell_of(6), 2);
    }

    /// Source bytes map to the shown text a window lays out, and back from where the mouse
    /// lands in it.
    #[test]
    fn bytes_map_to_the_shown_text() {
        // `a` shows at 0, the override (three bytes) as `<U+202E>` at 1..9, `b` at 9.
        let line = DisplayLine::new("a\u{202e}b");
        assert_eq!(line.shown(0..1), 0..1);
        assert_eq!(line.shown(1..4), 1..9);
        assert_eq!(line.shown(4..5), 9..10);
        assert_eq!(line.byte_at_shown(0), 0);
        assert_eq!(line.byte_at_shown(5), 1);
        assert_eq!(line.byte_at_shown(9), 4);
        assert_eq!(line.byte_at_shown(10), 5);
        assert_eq!(line.byte_at_shown(40), 5);
    }

    /// Styled runs move onto the shown text whole parts at a time, still sorted and apart,
    /// so a window can style a placeholder as the character it stands for.
    #[test]
    fn runs_move_onto_the_shown_text() {
        let style = crate::paint::Style::default();
        let run = |range: Range<usize>| Run { range, style };
        let line = DisplayLine::new("a\u{202e}⚠\u{fe0f}b");
        let ranges: Vec<Range<usize>> = line
            .shown_runs([run(0..4), run(4..7), run(7..10), run(10..11)])
            .into_iter()
            .map(|run| run.range)
            .collect();
        // The override shows as eight bytes; a run over half the sign's cluster takes all of
        // it, and the one over the other half gives way.
        assert_eq!(ranges, [0..9, 9..15, 15..16]);
    }

    /// A press on any cell of a wide character or a placeholder lands at its start, and one
    /// past the end at the line's end.
    #[test]
    fn cells_map_back_to_bytes() {
        let line = DisplayLine::new("a漢\u{1b}e\u{301}");
        // `a` 0, `漢` 1..3, `^[` 3..5, `é` as `e` and a combining accent at 5.
        assert_eq!(line.byte_at(0), 0);
        assert_eq!(line.byte_at(1), 1);
        assert_eq!(line.byte_at(2), 1);
        assert_eq!(line.byte_at(3), 4);
        assert_eq!(line.byte_at(4), 4);
        assert_eq!(line.byte_at(5), 5);
        assert_eq!(line.byte_at(6), "a漢\u{1b}e\u{301}".len());
        assert_eq!(line.byte_at(40), "a漢\u{1b}e\u{301}".len());
    }

    /// A terminal draws a cluster in its first cell and a placeholder a character a cell.
    #[test]
    fn glyphs_are_what_each_cell_draws() {
        let line = DisplayLine::new("⚠\u{fe0f}\u{1b}\t");
        let drawn: Vec<(&str, usize, usize)> = line
            .glyphs()
            .map(|glyph| (glyph.text, glyph.cell, glyph.cells))
            .collect();
        assert_eq!(
            drawn,
            [
                ("⚠\u{fe0f}", 0, 2),
                ("^", 2, 1),
                ("[", 3, 1),
                (" ", 4, 1),
                (" ", 5, 1),
                (" ", 6, 1),
                (" ", 7, 1),
            ]
        );
    }

    /// The longest start that fits never splits a cluster or a placeholder, and takes the
    /// first part even when it is wider than the room.
    #[test]
    fn fitting_starts_keep_parts_whole() {
        let line = DisplayLine::new("ab漢\u{202e}c");
        assert_eq!(line.fitting(0), 1);
        assert_eq!(line.fitting(2), 2);
        assert_eq!(line.fitting(3), 2);
        assert_eq!(line.fitting(4), 5);
        assert_eq!(line.fitting(11), 5);
        assert_eq!(line.fitting(12), 8);
        assert_eq!(line.fitting(13), 9);
        assert_eq!(DisplayLine::new("漢").fitting(1), 3);
    }
}
