//! The tool's own rows in the diff: hunk headers, the rules of unchanged lines left out,
//! notes, moves and one-line descriptions.
//!
//! They sit on the band without a gutter, in muted colors, so they never read as code.

use review_core::UnitId;

use super::code::Frame;
use super::palette::HunkTone;
use super::{Palette, Piece, Weight};
use crate::color::Hsla;
use crate::model::Model;
use crate::rows::{Badge, Note, Row, RowKind};
use crate::state;

/// One of the tool's own rows, as it draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    /// Air between hunks: nothing.
    Spacer,
    /// Unchanged lines left out: a hairline rule interrupted by the count, such as
    /// `15 unchanged lines`; it shows more when pressed.
    Gap {
        /// The count.
        label: Piece,
        /// The rule's color.
        rule: Hsla,
    },
    /// A hunk's header.
    Header(HunkHeader),
    /// A note: italic yellow text with a yellow bar along its left, on a block tinted toward
    /// the yellow, so it never reads as code.
    Note {
        /// The note.
        text: Piece,
        /// Who wrote it or what it is about, after it.
        tag: Option<Piece>,
        /// The bar's color.
        accent: Hsla,
        /// The block's background.
        background: Hsla,
    },
    /// A move, a collapsed unit or a description: one quiet line on the band.
    Line {
        /// The line.
        text: Piece,
        /// What Enter does on it, after it.
        hint: Option<Piece>,
        /// The line's background.
        background: Hsla,
    },
}

/// The cells of space before a hunk header's location and tag, which set them apart as
/// asides.
pub const TAG_GAP_CELLS: u16 = 4;

/// A hunk's header: one quiet line with its state glyph, the scopes around it, where it is when
/// its code is folded away or no scope names it, and what kind of change it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkHeader {
    /// The line's background: a gray a step above its hunk's band, so each hunk's start shows
    /// at a glance, tinted green once approved and red once flagged; on the selected hunk, the
    /// same solid blue, green or red as its bar.
    pub background: Hsla,
    /// `▸` when the hunk shows its header alone, `▾` when its code shows, in the gutter.
    pub fold: Piece,
    /// The review state's glyph, in the sign column.
    pub glyph: Piece,
    /// The scopes around the hunk, such as `impl Ledger › fn apply`.
    pub scope: Option<Scope>,
    /// The lines it covers, such as `L27–29`: quiet text on the header's own background,
    /// [`TAG_GAP_CELLS`] apart from the scopes before it, as the tag is.
    pub location: Option<Piece>,
    /// What kind of change it is, such as `(reformatted only)`: an italic aside in the faint
    /// color, last on the line and [`TAG_GAP_CELLS`] apart from what comes before it.
    pub tag: Option<Piece>,
}

/// A hunk header's scopes, in bold: the outer ones faint and cut first when space runs out,
/// the innermost bright, or dim once the hunk is approved and folded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The outer scopes with the separator after them, such as `impl Ledger › `.
    pub outer: Option<Piece>,
    /// The innermost scope, such as `fn apply`.
    pub inner: Piece,
}

/// How one of the tool's own rows moves with the diff's code, in code cells: as far as the
/// code moved, but no further than the row runs past its room, so a row that fits never moves
/// and where a hunk is and where a move goes stay in view while the code pans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NotePan {
    /// How far the row's text has moved left.
    pub shift: f64,
    /// Whether some of the row is still cut at its room's right edge, where the clip marker
    /// shows.
    pub cut: bool,
}

impl NotePan {
    /// How a row that runs `overflow` code cells past its room moves once the code has moved
    /// `offset` cells left.
    #[must_use]
    pub fn new(offset: f64, overflow: f64) -> Self {
        let shift = offset.min(overflow);
        Self {
            shift,
            cut: shift < overflow,
        }
    }
}

/// How one of the tool's own rows moves with the diff's code now, when what the front end
/// draws of it, unmoved, reaches `reach` past where code starts, in the units of
/// [`Sizes`](crate::frame::Sizes): pixels in review-gui, cells in review-tui.
///
/// Both front ends draw every one of the tool's own rows by it. How far the code can pan
/// comes from [`Note::width`], an upper bound of every row's reach, so the pan always reaches
/// the end of every row; the row itself moves and shows its marker only by what it really
/// runs past, so one that fits never moves or shows the marker.
#[must_use]
pub fn note_pan(model: &Model, reach: f32) -> NotePan {
    let overflow = model.geometry().reach_overflow(&model.sizes(), reach);
    NotePan::new(model.diff_text().offset(), overflow)
}

/// Diff row `row`, one of the tool's own rows showing `note`, in `frame`.
#[must_use]
pub fn annotation(
    model: &Model,
    palette: &Palette,
    row: &Row,
    note: &Note,
    frame: Frame,
) -> Annotation {
    match note.kind {
        RowKind::Spacer => Annotation::Spacer,
        RowKind::Gap => Annotation::Gap {
            label: Piece::new(note.text.clone(), palette.faint),
            rule: palette.hairline,
        },
        RowKind::Header => Annotation::Header(hunk_header(model, palette, row, note, frame)),
        RowKind::Note => Annotation::Note {
            text: Piece::new(note.text.clone(), palette.note).italicized(),
            tag: note
                .tag
                .as_ref()
                .map(|tag| Piece::new(tag.clone(), palette.dim)),
            accent: palette.note,
            background: palette.note_band,
        },
        _ => {
            let color = match note.kind {
                RowKind::Moved => palette.moved,
                _ => palette.dim,
            };
            Annotation::Line {
                text: Piece::new(note.text.clone(), color),
                hint: note.hint().map(|hint| Piece::new(hint, palette.faint)),
                background: palette.band,
            }
        }
    }
}

/// The glyph of a hunk's state, and the badge whose color it takes.
#[must_use]
pub fn badges_glyph(badges: &[Badge]) -> (Badge, &'static str) {
    let all = |wanted: fn(Badge) -> bool| !badges.is_empty() && badges.iter().copied().all(wanted);
    if badges.contains(&Badge::Flagged) {
        (Badge::Flagged, "!")
    } else if badges.contains(&Badge::Changed) {
        (Badge::Changed, "↻")
    } else if all(|badge| badge == Badge::Auto) {
        (Badge::Auto, "✓")
    } else if all(state::is_approved) {
        (Badge::Accepted, "✓")
    } else if badges.iter().copied().any(state::is_approved) {
        (Badge::Unreviewed, "◐")
    } else {
        (Badge::Unreviewed, "○")
    }
}

/// Whether `units` are all chunks of text, so their header's text is only where they are.
fn all_chunks(model: &Model, units: &[UnitId]) -> bool {
    model.view().is_some_and(|view| {
        !units.is_empty()
            && units.iter().all(|&unit| {
                view.unit(unit)
                    .ok()
                    .and_then(|record| record.chunk())
                    .is_some()
            })
    })
}

/// The header of the hunk that `row` heads.
fn hunk_header(
    model: &Model,
    palette: &Palette,
    row: &Row,
    note: &Note,
    frame: Frame,
) -> HunkHeader {
    let units = row
        .hunk
        .and_then(|hunk| model.rows.hunks.get(hunk))
        .cloned()
        .unwrap_or_default();
    let badges = model
        .view()
        .map(|view| state::badges(&view, &units, &model.pending))
        .unwrap_or_default();
    let (badge, glyph) = badges_glyph(&badges);
    let folded = model
        .view()
        .is_some_and(|view| units.iter().all(|&unit| model.folds.is_folded(&view, unit)));
    let tone = HunkTone::of(&badges);
    let selected = matches!(frame, Frame::Selected(_));
    let (background, palette) = if selected {
        let solid = palette.hunk_solid(tone);
        (solid, palette.on(solid))
    } else {
        (palette.hunk_header(tone), palette.on_header(tone))
    };
    let palette = &palette;
    // What the hunk changes leads: the scopes around it, innermost brightest.
    // Where it is matters only when its code is folded away or no scope names it, since the
    // gutter numbers the lines that show; a file unit's text says what changed, so it stays.
    let receding = folded && matches!(badge, Badge::Accepted | Badge::Auto);
    let label = if receding { palette.faint } else { palette.dim };
    let lines_only = all_chunks(model, &units);
    let scope = note
        .scope
        .as_deref()
        .map(|scope| scope_title(scope, receding, palette));
    let location = (scope.is_none() || folded || !lines_only).then(|| note.text.clone());
    let location_color = if scope.is_some() && lines_only {
        palette.faint
    } else {
        label
    };
    let location = location.map(|text| Piece::new(text, location_color));
    let tag = note
        .tag
        .as_ref()
        .map(|tag| Piece::new(format!("({tag})"), palette.faint).italicized());
    HunkHeader {
        background,
        fold: Piece::new(if folded { "▸" } else { "▾" }, palette.faint),
        glyph: Piece::new(glyph, palette.badge(badge)),
        scope,
        location,
        tag,
    }
}

/// A hunk header's scopes, such as `impl Ledger › fn apply`, split so the innermost reads
/// brightest.
fn scope_title(scope: &str, receding: bool, palette: &Palette) -> Scope {
    let (outer, inner) = match scope.rsplit_once(" › ") {
        Some((outer, inner)) => (Some(format!("{outer} › ")), inner),
        None => (None, scope),
    };
    Scope {
        outer: outer.map(|outer| Piece::new(outer, palette.faint).weighted(Weight::Bold)),
        inner: Piece::new(inner, if receding { palette.dim } else { palette.text })
            .weighted(Weight::Bold),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A flag or a change since review wins over approval, and a partly approved hunk shows
    /// half a circle.
    #[test]
    fn hunk_glyphs_name_what_needs_the_reviewer() {
        assert_eq!(badges_glyph(&[]), (Badge::Unreviewed, "○"));
        assert_eq!(
            badges_glyph(&[Badge::Accepted, Badge::Flagged]),
            (Badge::Flagged, "!")
        );
        assert_eq!(
            badges_glyph(&[Badge::Seen, Badge::Changed]),
            (Badge::Changed, "↻")
        );
        assert_eq!(
            badges_glyph(&[Badge::Seen, Badge::Accepted]),
            (Badge::Accepted, "✓")
        );
        assert_eq!(
            badges_glyph(&[Badge::Accepted, Badge::Unreviewed]),
            (Badge::Unreviewed, "◐")
        );
        // Approval without a mark keeps its own, quieter color, until a mark joins it.
        assert_eq!(badges_glyph(&[Badge::Auto]), (Badge::Auto, "✓"));
        assert_eq!(
            badges_glyph(&[Badge::Auto, Badge::Accepted]),
            (Badge::Accepted, "✓")
        );
        assert_eq!(
            badges_glyph(&[Badge::Auto, Badge::Unreviewed]),
            (Badge::Unreviewed, "◐")
        );
    }

    /// The innermost scope stands apart from the outer ones, which keep the separator.
    #[test]
    fn scopes_split_at_the_innermost() {
        let (themes, _) = crate::theme::all(None);
        let Some(theme) = themes.first() else {
            panic!("built-in themes");
        };
        let palette = Palette::new(theme, None, None, crate::config::Background::Bold);
        let scope = scope_title("impl Ledger › fn apply", false, &palette);
        assert_eq!(
            scope.outer.map(|outer| outer.text),
            Some("impl Ledger › ".to_owned())
        );
        assert_eq!(scope.inner.text, "fn apply");
        assert_eq!(scope.inner.color, palette.text);
        let alone = scope_title("fn main", true, &palette);
        assert_eq!(alone.outer, None);
        assert_eq!(alone.inner.color, palette.dim);
    }
}
