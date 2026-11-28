//! A changed PDF's pages: each version's page text, read within fixed bounds, the pairs of
//! pages the navigator steps through with each pair's text diff, and one page pair rendered
//! for the image comparison.
//!
//! pdfium reads and renders, behind the `pdf` feature, and is loaded the first time a PDF is
//! read, never before; without the feature a changed PDF is a binary file.
//! Every bound holds before the memory it guards is taken: the blob's size before it is read,
//! the page count before any page, and each page's characters before its text is copied.
//!
//! A page's text is compared as extracted, in order, with only its line-break encodings made
//! one: nothing else is dropped, joined or reordered before deciding whether two pages say the
//! same, so different extracts never compare equal. Its words are then aligned in order, and the
//! changes read a sentence a row, with the words that changed marked and characters that never
//! show written out as the code rows write them. Pages that differ only in a line break on one
//! side where the other has a space say only their line breaks differ; every other whitespace
//! shows. Pages whose sentences only moved, each exactly once, show the moved sentences where
//! they are now.
//!
//! Pages pair by their text: a text that is on exactly one page of each version, and not
//! blank, anchors those two pages to each other, and a pair whose order reverses against
//! another's was reordered. Between the anchors in order, the pages left pair by position
//! where both versions have as many, show as added or removed where only one has any, and as
//! unmatched otherwise. Duplicated and blank pages, such as scanned ones, never anchor.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use review_core::analysis::Chunk;

use crate::images::{Decoded, Version};
use crate::rows::{MovedPassage, PageRows};

/// The most pages a version may have to be read.
pub const MAX_PAGES: u32 = 500;

/// The most characters one page may hold to be read.
#[cfg_attr(
    not(feature = "pdf"),
    expect(
        dead_code,
        reason = "only pdfium's reading meets it, which the `pdf` feature builds"
    )
)]
pub const MAX_PAGE_CHARS: usize = 100_000;

/// The most characters a version's pages may hold together to be read.
#[cfg_attr(
    not(feature = "pdf"),
    expect(
        dead_code,
        reason = "only pdfium's reading meets it, which the `pdf` feature builds"
    )
)]
pub const MAX_CHARS: usize = 1_000_000;

/// The resolution a page renders at, in dots per inch, within the comparison's display edge.
#[cfg(feature = "pdf")]
const DPI: u16 = 144;

/// Whether `bytes` are a PDF, by its header.
#[must_use]
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF-")
}

/// Why a changed PDF shows as a binary file.
#[cfg_attr(
    not(feature = "pdf"),
    expect(
        dead_code,
        reason = "only pdfium's reading meets it, which the `pdf` feature builds"
    )
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fallback {
    /// A blob larger than the comparison reads, with its size in bytes.
    TooLarge(u64),
    /// More pages than [`MAX_PAGES`], with the count.
    TooManyPages(u32),
    /// A page with more characters than [`MAX_PAGE_CHARS`], or pages with more than
    /// [`MAX_CHARS`] together.
    TooMuchText,
    /// An encrypted PDF, whether or not it opens without a password.
    Encrypted,
    /// A blob that could not be read, or a PDF pdfium cannot load.
    Unreadable,
    /// pdfium could not be loaded, with why.
    NoPdfium(String),
}

impl Fallback {
    /// Why the version named `side` shows no pages, as the binary line says it.
    #[must_use]
    pub fn reason(&self, side: &str) -> String {
        match self {
            Self::TooLarge(bytes) => {
                let tenths = bytes / 100_000;
                format!(
                    "{side} too large to read ({}.{} MB)",
                    tenths / 10,
                    tenths % 10
                )
            }
            Self::TooManyPages(pages) => {
                format!("{side} has {pages} pages, more than {MAX_PAGES} to read")
            }
            Self::TooMuchText => format!("{side} has too much text to read"),
            Self::Encrypted => format!("{side} is encrypted"),
            Self::Unreadable => format!("{side} could not be read as a PDF"),
            Self::NoPdfium(why) => format!("PDF support failed to start: {why}"),
        }
    }
}

/// How one page pair's texts compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// Both pages have the same text.
    Same,
    /// Neither page has any text, as a scanned page has none.
    NoText,
    /// The pages have the same words in the same order, with line breaks where the other has
    /// spaces.
    LineBreaks,
    /// The pages have the same sentences, some of them moved, each exactly once.
    Moved,
    /// The pages' text changed.
    Changed,
    /// A page only the head has.
    Added,
    /// A page only the base has.
    Removed,
    /// Pages of the same text whose order among the pages reverses against another pair's.
    Reordered,
    /// A page with no page to pair with, among pages of both versions that do not pair up.
    Unmatched,
}

/// A page of each version, or of one, as the navigator steps through them.
#[derive(Debug, Clone, PartialEq)]
pub struct PagePair {
    /// The base's page, zero-based.
    pub base: Option<u32>,
    /// The head's page, zero-based.
    pub head: Option<u32>,
    /// How their texts compare.
    pub kind: PageKind,
    /// What their rows show: the changes from the base page's text to the head page's, a
    /// missing page's text empty, or the sentences that moved.
    pub rows: PageRows,
}

/// A changed PDF's pages, as reading both versions found them.
#[derive(Debug, Clone, PartialEq)]
pub struct Pages {
    /// How many pages the base has, none when it is absent.
    pub base: u32,
    /// How many pages the head has.
    pub head: u32,
    /// Every page of both, in pairs, in the head's order with the base's other pages where
    /// they fall.
    pub pairs: Vec<PagePair>,
}

impl Pages {
    /// The pairs of `base`'s and `head`'s page texts as extracted, each `None` for an absent
    /// version.
    #[must_use]
    pub fn of(base: Option<&[String]>, head: Option<&[String]>) -> Self {
        let read = |pages: Option<&[String]>| -> Vec<String> {
            pages
                .unwrap_or_default()
                .iter()
                .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
                .collect()
        };
        let (base, head) = (read(base), read(head));
        let count = |pages: &[String]| u32::try_from(pages.len()).unwrap_or(u32::MAX);
        let text = |pages: &[String], page: Option<usize>| -> String {
            page.and_then(|page| pages.get(page))
                .cloned()
                .unwrap_or_default()
        };
        let pairs = pair(&base, &head)
            .into_iter()
            .map(|(old, new, kind)| {
                let (old_text, new_text) = (text(&base, old), text(&head, new));
                let (compared, rows) = compare(&old_text, &new_text);
                PagePair {
                    base: old.and_then(|page| u32::try_from(page).ok()),
                    head: new.and_then(|page| u32::try_from(page).ok()),
                    // Only a pair of pages compares as a pair; a page alone is added, removed
                    // or unmatched whatever its text.
                    kind: if old.is_some() && new.is_some() && kind != PageKind::Reordered {
                        compared
                    } else {
                        kind
                    },
                    rows,
                }
            })
            .collect();
        Self {
            base: count(&base),
            head: count(&head),
            pairs,
        }
    }

    /// The pair to show first: the first whose text changed or that moved, else the first.
    #[must_use]
    pub fn first_changed(&self) -> usize {
        self.pairs
            .iter()
            .position(|pair| {
                !matches!(
                    pair.kind,
                    PageKind::Same | PageKind::NoText | PageKind::LineBreaks
                )
            })
            .unwrap_or(0)
    }

    /// What the navigator says of pair `index`, such as `Base 2/8 → Head 4/9 · reordered`.
    #[must_use]
    pub fn describe(&self, index: usize) -> Option<String> {
        let pair = self.pairs.get(index)?;
        let side = |name: &str, page: Option<u32>, count: u32| {
            page.map(|page| format!("{name} {}/{count}", page + 1))
        };
        let sides: Vec<String> = [
            side("Base", pair.base, self.base),
            side("Head", pair.head, self.head),
        ]
        .into_iter()
        .flatten()
        .collect();
        let kind = match pair.kind {
            PageKind::Same => "extracted text identical",
            PageKind::NoText => "no text extracted",
            PageKind::LineBreaks => "only the line breaks differ",
            PageKind::Moved => "same text, different order",
            PageKind::Changed => "text changed",
            PageKind::Added => "added page",
            PageKind::Removed => "removed page",
            PageKind::Reordered => "reordered · extracted text identical",
            PageKind::Unmatched => "no page to compare with",
        };
        Some(format!("{} · {kind}", sides.join(" → ")))
    }
}

/// How often a text is on a page of each version, and the last page it is on in each.
#[derive(Default)]
struct Seen {
    base: usize,
    head: usize,
    base_page: usize,
    head_page: usize,
}

/// Pairs the pages `base` and `head`, by index, as the module says; each pair's kind is
/// [`PageKind::Same`] until [`compare`] says how its texts compare.
fn pair(base: &[String], head: &[String]) -> Vec<(Option<usize>, Option<usize>, PageKind)> {
    // The texts on exactly one page of each version, not blank.
    let mut seen: HashMap<&str, Seen> = HashMap::new();
    for (page, text) in base.iter().enumerate() {
        let entry = seen.entry(text).or_default();
        entry.base += 1;
        entry.base_page = page;
    }
    for (page, text) in head.iter().enumerate() {
        let entry = seen.entry(text).or_default();
        entry.head += 1;
        entry.head_page = page;
    }
    let mut anchors: Vec<(usize, usize)> = seen
        .iter()
        .filter(|&(text, seen)| seen.base == 1 && seen.head == 1 && !text.trim().is_empty())
        .map(|(_, seen)| (seen.base_page, seen.head_page))
        .collect();
    anchors.sort_unstable();
    let reordered: HashSet<(usize, usize)> = anchors
        .iter()
        .filter(|&&(old, new)| {
            anchors
                .iter()
                .any(|&(other_old, other_new)| (old < other_old) != (new < other_new))
        })
        .copied()
        .collect();
    let matched_base: HashSet<usize> = anchors.iter().map(|&(old, _)| old).collect();
    let moved: HashMap<usize, usize> = reordered.iter().map(|&(old, new)| (new, old)).collect();
    // Each gap before an anchor in order, then the anchor; the last gap ends with the pages.
    let end = (base.len(), head.len());
    let ends: Vec<(usize, usize)> = anchors
        .iter()
        .filter(|anchor| !reordered.contains(anchor))
        .copied()
        .chain([end])
        .collect();
    let mut pairs = Vec::new();
    let mut from = (0, 0);
    for (old, new) in ends {
        let free_base: Vec<usize> = (from.0..old)
            .filter(|page| !matched_base.contains(page))
            .collect();
        let free_head: Vec<usize> = (from.1..new)
            .filter(|page| !moved.contains_key(page))
            .collect();
        let mut by_head: HashMap<usize, (Option<usize>, PageKind)> = HashMap::new();
        if free_base.len() == free_head.len() {
            for (&old, &new) in free_base.iter().zip(&free_head) {
                by_head.insert(new, (Some(old), PageKind::Same));
            }
        } else {
            let (removed, added) = if free_base.is_empty() || free_head.is_empty() {
                (PageKind::Removed, PageKind::Added)
            } else {
                (PageKind::Unmatched, PageKind::Unmatched)
            };
            pairs.extend(free_base.iter().map(|&old| (Some(old), None, removed)));
            for &new in &free_head {
                by_head.insert(new, (None, added));
            }
        }
        for page in from.1..new {
            if let Some(&old) = moved.get(&page) {
                pairs.push((Some(old), Some(page), PageKind::Reordered));
            } else if let Some(&(old, kind)) = by_head.get(&page) {
                pairs.push((old, Some(page), kind));
            }
        }
        if (old, new) != end {
            pairs.push((Some(old), Some(new), PageKind::Same));
        }
        from = (old + 1, new + 1);
    }
    pairs
}

/// How the page text `old` compares with `new`, each as extracted with its line breaks made
/// one, and the rows that show it.
fn compare(old: &str, new: &str) -> (PageKind, PageRows) {
    let none = || PageRows::Edits(Arc::new(Vec::new()));
    if old.trim().is_empty() && new.trim().is_empty() {
        return (PageKind::NoText, none());
    }
    if old == new {
        return (PageKind::Same, none());
    }
    // A line break where the other has a space is the only difference passed over; every other
    // whitespace shows, as extracted.
    let (old, new) = (old.replace('\n', " "), new.replace('\n', " "));
    if old == new {
        return (PageKind::LineBreaks, none());
    }
    let (old, new) = (rows(&old), rows(&new));
    let chunks = Chunk::between(&old, &new);
    match moved(&old, &new, &chunks) {
        Some(passages) => (PageKind::Moved, PageRows::Moved(Arc::new(passages))),
        None => (PageKind::Changed, PageRows::Edits(Arc::new(chunks))),
    }
}

/// The passages `chunks` only moved, from `old` to `new`, at their new places, when each of
/// them was taken out in one place and put back unchanged in one other, and its text is
/// nowhere else on either side; `None` when anything else changed or a match is ambiguous.
fn moved(old: &str, new: &str, chunks: &[Chunk]) -> Option<Vec<MovedPassage>> {
    let once = |text: &str, passage: &str| text.matches(passage).count() == 1;
    let removed: Vec<&Chunk> = chunks
        .iter()
        .filter(|chunk| chunk.added.is_empty())
        .collect();
    let added: Vec<&Chunk> = chunks
        .iter()
        .filter(|chunk| chunk.removed.is_empty())
        .collect();
    if removed.is_empty() || removed.len() + added.len() != chunks.len() {
        return None;
    }
    if removed.len() != added.len() {
        return None;
    }
    added
        .into_iter()
        .map(|arrived| {
            let passage = arrived.added.as_str();
            let left = removed
                .iter()
                .find(|left| left.removed == passage)
                .filter(|_| once(old, passage) && once(new, passage))?;
            Some(MovedPassage {
                chunk: arrived.clone(),
                from: left.old.start,
            })
        })
        .collect()
}

/// `text`, with no line breaks, as rows to read, a sentence a row: a row ends after `.`, `!`
/// or `?` before a space and more text, and takes that space. Nothing else changes, so two texts
/// have the same rows exactly when they are the same, and the rows draw characters that never
/// show as the code rows do.
fn rows(text: &str) -> String {
    let mut rows = String::new();
    let mut chars = text.chars().peekable();
    while let Some(char) = chars.next() {
        rows.push(char);
        if matches!(char, '.' | '!' | '?') && chars.next_if_eq(&' ').is_some() {
            // A space the text ends on stays, so `A. ` never reads as `A.`.
            rows.push(if chars.peek().is_some() { '\n' } else { ' ' });
        }
    }
    if !rows.is_empty() && !rows.ends_with('\n') {
        rows.push('\n');
    }
    rows
}

/// The text of each page of the PDF `bytes` as pdfium extracts it, within the bounds, or why
/// the PDF shows as a binary file instead; runs in the background.
///
/// Before each page it asks whether its text is still `wanted`, and once it is not, it stops
/// and returns `None`, letting go of pdfium for the next reader.
#[cfg(feature = "pdf")]
pub fn page_texts(
    bytes: &[u8],
    wanted: &dyn Fn() -> bool,
) -> Option<Result<Vec<String>, Fallback>> {
    let pdfium = match pdfium_embed::Pdfium::instance() {
        Ok(pdfium) => pdfium,
        Err(error) => return Some(Err(Fallback::NoPdfium(error.to_string()))),
    };
    // The document's lock is held only while reading, never while diffing.
    pdfium
        .with_document(bytes, |document| {
            if document.is_encrypted() {
                return Err(Fallback::Encrypted);
            }
            let pages = document.num_pages();
            if pages > MAX_PAGES {
                return Err(Fallback::TooManyPages(pages));
            }
            let mut left = MAX_CHARS;
            let mut texts = Vec::new();
            for page in 0..pages {
                if !wanted() {
                    return Ok(None);
                }
                let text = document.page_text_limited(page, MAX_PAGE_CHARS.min(left))?;
                left = left.saturating_sub(text.chars().count());
                texts.push(text);
            }
            Ok(Some(texts))
        })
        .transpose()
}

/// Without the `pdf` feature, no PDF is read: a PDF still wanted shows as a binary file.
#[cfg(not(feature = "pdf"))]
pub fn page_texts(_: &[u8], wanted: &dyn Fn() -> bool) -> Option<Result<Vec<String>, Fallback>> {
    wanted().then(|| Err(Fallback::NoPdfium("built without PDF support".to_owned())))
}

#[cfg(feature = "pdf")]
impl From<pdfium_embed::DocumentError> for Fallback {
    fn from(error: pdfium_embed::DocumentError) -> Self {
        match error {
            pdfium_embed::DocumentError::Encrypted(_) => Self::Encrypted,
            pdfium_embed::DocumentError::PageTextTooLong { .. } => Self::TooMuchText,
            _ => Self::Unreadable,
        }
    }
}

/// Page `page` of the PDF `bytes` rendered within the comparison's display edge, or what shows
/// in its place; runs in the background.
///
/// Its size is the page's in points, so pages of other sizes are never compared
/// pixel by pixel, even where both renders reach the display edge at one size.
pub(crate) fn render(bytes: &[u8], page: u32) -> Result<Decoded, Version> {
    let format = format!("PDF page {}", page + 1);
    let Some((image, points)) = render_page(bytes, page) else {
        return Err(Version::Unreadable(Some(format)));
    };
    Ok(Decoded::sized(format, image, points))
}

/// Page `page` of the PDF `bytes` rendered on white, at most the display edge on its long side,
/// and the page's width and height in points.
#[cfg(feature = "pdf")]
fn render_page(bytes: &[u8], page: u32) -> Option<(image::RgbaImage, [f32; 2])> {
    let pdfium = pdfium_embed::Pdfium::instance().ok()?;
    pdfium
        .with_document(bytes, |document| {
            if document.is_encrypted() {
                return Err(pdfium_embed::DocumentError::InvalidRenderDimensions);
            }
            document.render_page(page, DPI, crate::images::DISPLAY_EDGE)
        })
        .ok()
        .map(|rendered| (rendered.image, [rendered.width_pts, rendered.height_pts]))
}

/// Without the `pdf` feature, no page renders.
#[cfg(not(feature = "pdf"))]
fn render_page(_: &[u8], _: u32) -> Option<(image::RgbaImage, [f32; 2])> {
    None
}

/// Tiny PDFs written out for tests, which only pdfium reads.
#[cfg(all(test, feature = "pdf"))]
pub(crate) mod testing {
    use std::fmt::Write as _;

    /// A PDF of one page for each of `pages`, each line of a page's text on a line of its own.
    pub(crate) fn pdf(pages: &[&str]) -> Vec<u8> {
        write(pages, PAGE, false)
    }

    /// A PDF of `pages`, each `width` × `height` points large.
    pub(crate) fn sized(pages: &[&str], width: f32, height: f32) -> Vec<u8> {
        write(pages, (width, height), false)
    }

    /// The `MediaBox` size the other writers give a page, in points.
    const PAGE: (f32, f32) = (200.0, 280.0);

    /// A PDF of `pages` that asks for a password no one knows.
    pub(crate) fn encrypted(pages: &[&str]) -> Vec<u8> {
        write(pages, PAGE, true)
    }

    fn write(pages: &[&str], (width, height): (f32, f32), encrypted: bool) -> Vec<u8> {
        // Objects: 1 the catalog, 2 the page tree, 3 the font, then each page and its content.
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            String::new(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut kids = Vec::new();
        for text in pages {
            let page = objects.len() + 1;
            kids.push(format!("{page} 0 R"));
            let mut content = "BT /F1 12 Tf 14 TL 20 260 Td".to_owned();
            for line in text.lines() {
                let escaped = line
                    .replace('\\', "\\\\")
                    .replace('(', "\\(")
                    .replace(')', "\\)");
                write!(content, " ({escaped}) Tj T*").ok();
            }
            content.push_str(" ET");
            objects.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] \
                 /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
                page + 1
            ));
            let length = format!("<< /Length {} >>", content.len());
            objects.push([length.as_str(), "stream", &content, "endstream"].join("\n"));
        }
        let tree = format!(
            "<< /Type /Pages /Kids [{}] /Count {} >>",
            kids.join(" "),
            pages.len()
        );
        if let Some(slot) = objects.get_mut(1) {
            *slot = tree;
        }
        // Encrypted with a password no one knows: the keys check no password.
        let mut encryption = String::new();
        if encrypted {
            let key = "ab".repeat(32);
            objects.push(format!(
                "<< /Filter /Standard /V 1 /R 2 /O <{key}> /U <{key}> /P -44 >>"
            ));
            let id = "01".repeat(16);
            encryption = format!(" /Encrypt {} 0 R /ID [<{id}> <{id}>]", objects.len());
        }
        let trailer = format!("/Size {} /Root 1 0 R{encryption}", objects.len() + 1);
        let mut pdf = "%PDF-1.4\n".to_owned();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            writeln!(pdf, "{} 0 obj", index + 1).ok();
            writeln!(pdf, "{object}").ok();
            writeln!(pdf, "endobj").ok();
        }
        let xref = pdf.len();
        writeln!(pdf, "xref").ok();
        writeln!(pdf, "0 {}", objects.len() + 1).ok();
        writeln!(pdf, "0000000000 65535 f ").ok();
        for offset in offsets {
            writeln!(pdf, "{offset:010} 00000 n ").ok();
        }
        for line in [
            "trailer",
            &format!("<< {trailer} >>"),
            "startxref",
            &xref.to_string(),
            "%%EOF",
        ] {
            writeln!(pdf, "{line}").ok();
        }
        pdf.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Page texts as the reading finds them.
    fn texts(pages: &[&str]) -> Vec<String> {
        pages.iter().map(|page| (*page).to_owned()).collect()
    }

    /// Each pair's pages, one-based as the navigator numbers them, and kind.
    fn paired(base: &[&str], head: &[&str]) -> Vec<(Option<u32>, Option<u32>, PageKind)> {
        Pages::of(Some(&texts(base)), Some(&texts(head)))
            .pairs
            .into_iter()
            .map(|pair| {
                (
                    pair.base.map(|page| page + 1),
                    pair.head.map(|page| page + 1),
                    pair.kind,
                )
            })
            .collect()
    }

    /// An inserted page only shifts the pages after it, which stay in order, and a page whose
    /// text changed pairs by position between the anchors around it.
    #[test]
    fn an_inserted_page_is_added_and_the_rest_stay_in_order() {
        use PageKind::{Added, Changed, Same, Unmatched};
        assert_eq!(
            paired(&["a\n", "b\n", "c\n"], &["a\n", "new\n", "b\n", "c\n"]),
            [
                (Some(1), Some(1), Same),
                (None, Some(2), Added),
                (Some(2), Some(3), Same),
                (Some(3), Some(4), Same),
            ]
        );
        assert_eq!(
            paired(&["a\n", "b\n", "c\n"], &["a\n", "b2\n", "c\n"]),
            [
                (Some(1), Some(1), Same),
                (Some(2), Some(2), Changed),
                (Some(3), Some(3), Same),
            ]
        );
        // A changed page beside an inserted one has no anchor to pair by, so both versions'
        // pages there are unmatched.
        assert_eq!(
            paired(&["a\n", "b\n", "c\n"], &["a\n", "new\n", "b2\n", "c\n"]),
            [
                (Some(1), Some(1), Same),
                (Some(2), None, Unmatched),
                (None, Some(2), Unmatched),
                (None, Some(3), Unmatched),
                (Some(3), Some(4), Same),
            ]
        );
    }

    /// Two pages that swap places are both reordered, while the pages around them stay in
    /// order, and the navigator says so.
    #[test]
    fn swapped_pages_are_reordered() {
        use PageKind::{Reordered, Same};
        let base = ["a\n", "b\n", "c\n", "d\n"];
        let head = ["a\n", "c\n", "b\n", "d\n"];
        assert_eq!(
            paired(&base, &head),
            [
                (Some(1), Some(1), Same),
                (Some(3), Some(2), Reordered),
                (Some(2), Some(3), Reordered),
                (Some(4), Some(4), Same),
            ]
        );
        let pages = Pages::of(Some(&texts(&base)), Some(&texts(&head)));
        assert_eq!(pages.first_changed(), 1);
        assert_eq!(
            pages.describe(1).as_deref(),
            Some("Base 3/4 → Head 2/4 · reordered · extracted text identical")
        );
    }

    /// Blank pages, as scanned ones read, and duplicated pages anchor nothing: they pair by
    /// position where the counts agree and are unmatched where they do not, and every page
    /// stays reachable.
    #[test]
    fn blank_and_duplicated_pages_never_anchor() {
        use PageKind::{NoText, Same, Unmatched};
        assert_eq!(
            paired(&["", ""], &["", ""]),
            [(Some(1), Some(1), NoText), (Some(2), Some(2), NoText)]
        );
        assert_eq!(
            paired(&["", "", "x\n"], &["", "x\n"]),
            [
                (Some(1), None, Unmatched),
                (Some(2), None, Unmatched),
                (None, Some(1), Unmatched),
                (Some(3), Some(2), Same),
            ]
        );
        let unmatched = paired(&["dup\n", "dup\n"], &["dup\n", "dup\n", "dup\n"]);
        assert_eq!(
            unmatched,
            [
                (Some(1), None, Unmatched),
                (Some(2), None, Unmatched),
                (None, Some(1), Unmatched),
                (None, Some(2), Unmatched),
                (None, Some(3), Unmatched),
            ]
        );
    }

    /// The kind and rows of one page pair, base `old` and head `new`.
    fn compared(old: &str, new: &str) -> (PageKind, PageRows) {
        let pages = Pages::of(Some(&texts(&[old])), Some(&texts(&[new])));
        let pair = pages.pairs.into_iter().next().unwrap();
        (pair.kind, pair.rows)
    }

    /// The changes rows show, or a panic naming what shows instead.
    fn edits(rows: &PageRows) -> &[Chunk] {
        match rows {
            PageRows::Edits(chunks) => chunks,
            PageRows::Moved(_) => panic!("passages moved: {rows:?}"),
        }
    }

    /// Extracts that differ never compare equal: a line break that splits a word or a token,
    /// labels and values that swapped, and a character that never shows each show a change.
    #[test]
    fn different_extracts_never_compare_equal() {
        let lines = |lines: &[&str]| lines.join("\n");
        for (old, new) in [
            (lines(&["They re-", "signed."]), lines(&["They resigned."])),
            (lines(&["Pass -", "force."]), lines(&["Pass -force."])),
            (
                lines(&["From", "Paris", "To", "Rome"]),
                lines(&["From", "Rome", "To", "Paris"]),
            ),
            (lines(&["ab"]), lines(&["a\u{200c}b"])),
            // A joiner and the same written out are other text.
            (lines(&["a\u{200c}b"]), lines(&["a⟨U+200C⟩b"])),
            // Whitespace other than a line break shows.
            (lines(&["A B"]), lines(&["A  B"])),
            (lines(&["A\tB"]), lines(&["A B"])),
            // A space after the last sentence is text too.
            (lines(&["A."]), lines(&["A. "])),
        ] {
            let (kind, rows) = compared(&old, &new);
            assert_eq!(kind, PageKind::Changed, "{old:?} → {new:?}");
            assert!(!edits(&rows).is_empty(), "{old:?} → {new:?}");
        }
        // The joiner is kept, for the rows to write out, never dropped.
        let (_, rows) = compared("ab", "a\u{200c}b");
        assert_eq!(edits(&rows)[0].added, "a\u{200c}b\n");
    }

    /// Identical text, text with nothing on it, and text wrapped onto other lines show no rows,
    /// and the navigator says which.
    #[test]
    fn rewrapped_and_identical_text_show_no_rows() {
        let wrapped = (
            &["The quick brown fox", "jumps over the dog."].join("\r\n"),
            &["The quick brown", "fox jumps over", "the dog."].join("\n"),
        );
        for ((old, new), kind, said) in [
            (
                (&"Same text.".to_owned(), &"Same text.".to_owned()),
                PageKind::Same,
                "extracted text identical",
            ),
            (
                (&String::new(), &" ".to_owned()),
                PageKind::NoText,
                "no text extracted",
            ),
            (wrapped, PageKind::LineBreaks, "only the line breaks differ"),
        ] {
            let pages = Pages::of(Some(&texts(&[old])), Some(&texts(&[new])));
            let pair = &pages.pairs[0];
            assert_eq!(pair.kind, kind);
            assert_eq!(edits(&pair.rows).len(), 0);
            assert_eq!(
                pages.describe(0),
                Some(format!("Base 1/1 → Head 1/1 · {said}"))
            );
        }
    }

    /// Paragraphs that swapped are the same text in another order: the one that moved shows
    /// where it is now, with where it was.
    #[test]
    fn swapped_paragraphs_are_moved() {
        let paragraphs = |first: &str, second: &str| [first, second].join("\n");
        let (kind, rows) = compared(
            &paragraphs("A one.", "B two."),
            &paragraphs("B two.", "A one."),
        );
        assert_eq!(kind, PageKind::Moved);
        let PageRows::Moved(passages) = rows else {
            panic!("nothing moved: {rows:?}");
        };
        assert_eq!(passages.len(), 1);
        assert_eq!(
            (passages[0].chunk.added.as_str(), passages[0].from),
            ("A one.\n", 0)
        );
        // A space more in the moved paragraph is an edit, never a move alone.
        let (kind, rows) = compared(
            &paragraphs("A one.", "B two."),
            &paragraphs("B two.", "A  one."),
        );
        assert_eq!(kind, PageKind::Changed);
        assert!(edits(&rows).iter().any(|chunk| chunk.added == "A  one.\n"));
        // A sentence that is there twice makes the move ambiguous: ordered edits instead.
        let (kind, _) = compared("A. B. A.", "B. A. A. C.");
        assert_eq!(kind, PageKind::Changed);
    }

    /// One changed word is one changed sentence, with only that word marked.
    #[test]
    fn a_changed_word_marks_only_itself() {
        let (kind, rows) = compared(
            "The fox is quick. It sleeps.",
            "The fox is slow. It sleeps.",
        );
        assert_eq!(kind, PageKind::Changed);
        let chunks = edits(&rows);
        assert_eq!(chunks.len(), 1);
        let chunk = &chunks[0];
        assert_eq!(
            (chunk.removed.as_str(), chunk.added.as_str()),
            ("The fox is quick.\n", "The fox is slow.\n")
        );
        let marked: Vec<&str> = chunk
            .added_highlights()
            .into_iter()
            .map(|range| &chunk.added[range])
            .collect();
        assert_eq!(marked, ["slow"]);
    }

    /// An added or deleted PDF is all added or removed pages, each its whole text.
    #[test]
    fn a_new_pdf_is_all_added_pages() {
        let pages = Pages::of(None, Some(&texts(&["one", "two"])));
        assert_eq!(pages.base, 0);
        assert!(pages.pairs.iter().all(|pair| pair.kind == PageKind::Added));
        assert_eq!(edits(&pages.pairs[1].rows)[0].added, "two\n");
        assert_eq!(pages.describe(1).as_deref(), Some("Head 2/2 · added page"));
    }

    /// pdfium reads each page's text, with its lines ending in newlines, refuses an encrypted
    /// PDF and one with too many pages, and renders a page within the display edge.
    #[cfg(feature = "pdf")]
    #[test]
    fn pdfium_reads_renders_and_refuses() {
        let pdf = testing::pdf(&[&["Hello", "world"].join("\n"), "second page"]);
        let wanted = || true;
        let read: Vec<String> = page_texts(&pdf, &wanted)
            .unwrap()
            .unwrap()
            .iter()
            .map(|text| text.replace("\r\n", "\n"))
            .collect();
        assert_eq!(
            read,
            [["Hello", "world"].join("\n"), "second page".to_owned()]
        );
        assert_eq!(
            page_texts(&testing::encrypted(&["secret"]), &wanted),
            Some(Err(Fallback::Encrypted))
        );
        let many = vec![""; usize::try_from(MAX_PAGES).unwrap() + 1];
        assert_eq!(
            page_texts(&testing::pdf(&many), &wanted),
            Some(Err(Fallback::TooManyPages(MAX_PAGES + 1)))
        );
        assert_eq!(
            page_texts(b"%PDF-1.4 broken", &wanted),
            Some(Err(Fallback::Unreadable))
        );
        // A read no longer wanted stops before its first page.
        assert_eq!(page_texts(&pdf, &|| false), None);
        let rendered = render(&pdf, 1).ok().unwrap();
        let version = crate::images::Comparison::between(Ok(rendered), Err(Version::Absent));
        let crate::images::Version::Image(picture) = version.base else {
            panic!("the page did not render");
        };
        assert_eq!(picture.size.format, "PDF page 2");
        assert_eq!((picture.size.width, picture.size.height), (200, 280));
        assert_eq!(picture.raster.dimensions(), (400, 560));
    }

    /// Pages of other sizes in one aspect, both rendered at the display edge, compare no
    /// pixels: their sizes differ.
    #[cfg(feature = "pdf")]
    #[test]
    fn pages_of_other_sizes_compare_no_pixels() {
        let compared = |base: (f32, f32), head: (f32, f32)| {
            let render = |(width, height)| render(&testing::sized(&["Same"], width, height), 0);
            crate::images::Comparison::between(render(base), render(head)).changes
        };
        assert_eq!(
            compared((2000.0, 2800.0), (4000.0, 5600.0)),
            crate::images::Changes::Sizes
        );
        // A tenth of a point apart: one raster size and one label, other pages all the same.
        assert_eq!(
            compared((600.1, 800.0), (600.2, 800.0)),
            crate::images::Changes::Sizes
        );
    }
}
