//! A changed image's comparison: what each version's label says, what it draws or says in its
//! place, and the line under both, which says what changed between their pixels.
//!
//! A comparison still being made paints its labels, and once it is slow a note in each place,
//! so a front end lays it out as it will be once it lands.
//! Where the comparison goes and how large its images draw is each front end's; both fit a
//! raster into its room with [`crate::images::fitted`].

use crate::images::{Changes, Comparison, Raster, Size, Version};
use crate::keymap::{self, Area};
use crate::model::Model;
use crate::paint::{Palette, Piece, Weight};

/// The comparison the selected file shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ComparisonPaint {
    /// The base's version, on the left.
    pub base: VersionPaint,
    /// The head's version, on the right.
    pub head: VersionPaint,
    /// Whether the head draws with its changed pixels tinted.
    pub tinted: bool,
    /// What changed between the pixels compared, with the key that shows or hides the changed
    /// pixels; empty when a version is not an image.
    pub status: Vec<Piece>,
}

/// One version's label and what it draws.
#[derive(Debug, Clone, PartialEq)]
pub struct VersionPaint {
    /// `base` or `head`.
    pub name: Piece,
    /// Its format and size, such as `PNG · 640 × 480`; empty when there is no image to
    /// describe.
    pub detail: Piece,
    /// The raster to draw, fitted into its room.
    pub raster: Option<Raster>,
    /// What shows in the raster's place, such as `not an image`.
    pub note: Option<Piece>,
}

/// The comparison the selected file shows, while one of its versions is an image or it may be
/// one.
#[must_use]
pub fn comparison(model: &Model, palette: &Palette) -> Option<ComparisonPaint> {
    let Some(compared) = model.comparison() else {
        let pending = model.pending_comparison()?;
        let waiting = |name: &str| VersionPaint {
            name: Piece::new(name, palette.text).weighted(Weight::Bold),
            detail: Piece::new("", palette.dim),
            raster: None,
            note: pending
                .slow
                .then(|| Piece::new("decoding…", palette.faint).italicized()),
        };
        return Some(ComparisonPaint {
            base: waiting("base"),
            head: waiting("head"),
            tinted: false,
            status: Vec::new(),
        });
    };
    let comparison = &compared.comparison;
    let tinted = match &comparison.changes {
        Changes::Pixels {
            tinted, changed, ..
        } if *changed > 0 && model.changed_pixels_shown() => Some(tinted),
        _ => None,
    };
    let mut head = version("head", &comparison.head, palette);
    if let Some(tinted) = tinted {
        head.raster = Some(std::sync::Arc::clone(tinted));
    }
    Some(ComparisonPaint {
        base: version("base", &comparison.base, palette),
        head,
        tinted: tinted.is_some(),
        status: status(model, comparison, palette),
    })
}

/// The label and raster of `version`, named `name`.
fn version(name: &str, version: &Version, palette: &Palette) -> VersionPaint {
    let described = |size: &Size| format!("{} · {} × {}", size.format, size.width, size.height);
    let (detail, raster, note) = match version {
        Version::Absent => {
            let note = if name == "base" {
                "none: the file is new"
            } else {
                "none: the file is deleted"
            };
            (String::new(), None, Some(note.to_owned()))
        }
        Version::Image(picture) => {
            let mut detail = described(&picture.size);
            if picture.first_frame {
                detail.push_str(" · first frame");
            }
            (detail, Some(std::sync::Arc::clone(&picture.raster)), None)
        }
        Version::TooLarge(bytes) => (
            megabytes(*bytes),
            None,
            Some("too large to preview".to_owned()),
        ),
        Version::TooManyPixels(size) => (
            described(size),
            None,
            Some("too many pixels to preview".to_owned()),
        ),
        Version::Unreadable(Some(format)) => (
            format.clone(),
            None,
            Some("could not be decoded".to_owned()),
        ),
        Version::Unreadable(None) => (String::new(), None, Some("could not be read".to_owned())),
        Version::NotImage => (String::new(), None, Some("not an image".to_owned())),
    };
    VersionPaint {
        name: Piece::new(name, palette.text).weighted(Weight::Bold),
        detail: Piece::new(detail, palette.dim),
        raster,
        note: note.map(|note| Piece::new(note, palette.faint).italicized()),
    }
}

/// The line under the comparison: what was compared and how many of its pixels changed, with
/// the key that shows them over the head, or why no pixels are compared.
fn status(model: &Model, comparison: &Comparison, palette: &Palette) -> Vec<Piece> {
    // Only an animation's first frame is decoded, so only first frames are compared.
    let first_frames = [&comparison.base, &comparison.head]
        .into_iter()
        .any(|version| matches!(version, Version::Image(picture) if picture.first_frame));
    let compared = if first_frames {
        "the first frames"
    } else {
        "the decoded images"
    };
    let mut pieces = Vec::new();
    let mut toggle = None;
    match &comparison.changes {
        Changes::Pixels { changed: 0, .. } => pieces.push(Piece::new(
            format!("{compared} show no pixel differences"),
            palette.dim,
        )),
        Changes::Pixels { changed, total, .. } => {
            let of = if first_frames {
                " of the first frames"
            } else {
                ""
            };
            pieces.push(Piece::new(
                format!(
                    "{changed} of {total} pixels{of} changed ({})",
                    share(*changed, *total)
                ),
                palette.text,
            ));
            toggle = toggle_key(model);
        }
        Changes::Sizes => pieces.push(Piece::new(
            "the sizes differ, so no pixels are compared",
            palette.dim,
        )),
        Changes::None => {}
    }
    if let Some(key) = toggle {
        let what = if model.changed_pixels_shown() {
            " hides them"
        } else {
            " tints them"
        };
        pieces.push(Piece::new(" · ", palette.faint));
        pieces.push(Piece::new(key, palette.accent));
        pieces.push(Piece::new(what, palette.dim));
    }
    pieces
}

/// The first key that shows or hides the changed pixels where the keyboard is, as the cheat
/// sheet writes it.
fn toggle_key(model: &Model) -> Option<String> {
    let area = match model.area() {
        Area::Tree => Area::Tree,
        _ => Area::Diff,
    };
    model
        .settings
        .keymap
        .iter()
        .find(|binding| binding.action == "ToggleChangedPixels" && binding.area == area)
        .and_then(|binding| binding.keys.first())
        .map(|key| keymap::display(key))
}

/// `part` of `whole` as a percentage with one decimal, such as `0.4%`, never rounded to none.
fn share(part: u64, whole: u64) -> String {
    let tenths = part.saturating_mul(1000) / whole.max(1);
    if tenths == 0 && part > 0 {
        return "under 0.1%".to_owned();
    }
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// `bytes` in megabytes with one decimal, such as `12.4 MB`.
fn megabytes(bytes: u64) -> String {
    let tenths = bytes / 100_000;
    format!("{}.{} MB", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shares and sizes read with one decimal, and a change too small for one still shows.
    #[test]
    fn numbers_read_with_one_decimal() {
        assert_eq!(share(4, 1000), "0.4%");
        assert_eq!(share(1000, 1000), "100.0%");
        assert_eq!(share(1, 1_000_000), "under 0.1%");
        assert_eq!(megabytes(12_400_000), "12.4 MB");
    }
}
