//! The selected file's pages when it is a PDF: reading both versions' page text in the
//! background, the page pair the navigator selects, and what the PDF's unit shows.
//!
//! A file whose content changed as one opaque unit is read once it shows, whether or not the
//! front end shows images, since the text diff needs none; only a PDF's pages are kept. The
//! same read decodes an image for the image comparison where the front end shows images, so
//! no blob is read twice. Without the `pdf` feature nothing is read here and every such file
//! is no PDF.
//!
//! Where the front end shows images, a PDF's selected page pair renders above its text until the
//! reviewer hides it, for the rest of the session; nothing renders while it is hidden.
//!
//! One job reads at a time, and one result is kept, for the pair read last. A result lands
//! only while its pair is still the one the selected file shows, and its first changed page
//! pair shows first. Review state is untouched: the PDF is one opaque unit, approved as a
//! whole, and its page rows are for reading.

use std::fmt::Write as _;
use std::sync::Arc;

use review_core::Review;
use review_core::analysis::{FileUnit, UnitKind};

use super::Model;
use super::compare::Compared;
use super::compare::{Page, Pair, read};
use super::effects::{Because, Outcome, Subject};
use crate::images::{Comparison, Encoded};
use crate::pdf::{Fallback, Pages, is_pdf, page_texts};
use crate::rows::{PdfRows, PdfShows};

/// What reading a changed file's versions found.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    /// Neither version is a PDF.
    NotPdf,
    /// The pages of both versions.
    Pages(Arc<Pages>),
    /// A PDF that shows as a binary file, and why.
    Binary(String),
    /// A version too large to read and none read as a PDF, and why: a PDF that says so when
    /// its path names one, and otherwise no PDF.
    TooLarge(String),
}

/// A pair, with what reading its versions found.
#[derive(Debug)]
pub struct ReadPdf {
    /// The pair read.
    pair: Pair,
    /// What reading it found.
    read: Read,
    /// What decoding its versions as images found, when the front end showed images, neither
    /// was a PDF and the pair was not compared already.
    pub(super) comparison: Option<Comparison>,
}

/// The selected PDF's pages, and the job that reads them.
#[derive(Debug, Default)]
pub(super) struct Pdfs {
    /// The pair the selected file shows, when its content is opaque.
    wanted: Option<Pair>,
    /// The pair read last, and what reading it found.
    read: Option<(Pair, Read)>,
    /// The pair a job reads now.
    reading: Option<Pair>,
    /// The page pair the navigator selects, by index into the pages read last.
    page: usize,
}

impl Pdfs {
    /// Whether a job reads a pair now.
    pub(super) fn busy(&self) -> bool {
        self.reading.is_some()
    }

    /// Notes that the job reading a pair ended, whether or not what it found lands.
    pub(super) fn ended(&mut self) {
        self.reading = None;
    }

    /// The pages read for the pair the selected file shows.
    fn pages(&self) -> Option<&Pages> {
        match &self.read {
            Some((pair, Read::Pages(pages))) if Some(pair) == self.wanted.as_ref() => Some(pages),
            _ => None,
        }
    }
}

/// Whether PDFs are read, as the `pdf` feature says.
const READS_PDFS: bool = cfg!(feature = "pdf");

impl Model {
    /// What reading `pair` found, once it was read; every file is no PDF without the `pdf`
    /// feature.
    pub(super) fn read_pdf(&self, pair: &Pair) -> Option<&Read> {
        if !READS_PDFS {
            return Some(&Read::NotPdf);
        }
        let (_, read) = self.pdfs.read.as_ref().filter(|(read, _)| read == pair)?;
        let named = self
            .selected
            .as_deref()
            .is_some_and(|path| path.to_ascii_lowercase().ends_with(".pdf"));
        match read {
            Read::TooLarge(_) if !named => Some(&Read::NotPdf),
            read => Some(read),
        }
    }

    /// Follows the pair the selected file shows, when its content is opaque, and starts
    /// reading it unless it is read or another pair is being read.
    pub(super) fn follow_pdf(&mut self, wanted: Option<&Pair>) {
        if !READS_PDFS {
            return;
        }
        self.pdfs.wanted = wanted.cloned();
        let Some(pair) = wanted.cloned() else {
            return;
        };
        if self.read_pdf(&pair).is_some() || self.pdfs.reading.is_some() {
            return;
        }
        self.pdfs.reading = Some(pair.clone());
        // An image compared before, as when the reviewer comes back to it, is not decoded again.
        let decode = self.shows_images() && !self.images.holds(&pair);
        // Keyed by blobs, whose content never changes, so it lands whatever shows meanwhile.
        self.spawn_with(Subject::Always(Because::Content), move |parts| {
            let (read, comparison) = read_pages(&parts.review, &pair, decode);
            Outcome::PdfRead(Box::new(ReadPdf {
                pair,
                read,
                comparison,
            }))
        });
    }

    /// Takes in what a job's reading found: keeps it while its pair is the one shown, showing
    /// its first changed page pair, and starts the pair shown now.
    pub(super) fn pdf_read(&mut self, read: ReadPdf) {
        let ReadPdf {
            pair,
            read,
            comparison,
        } = read;
        if self.pdfs.wanted.as_ref() == Some(&pair) {
            self.pdfs.page = match &read {
                Read::Pages(pages) => pages.first_changed(),
                Read::NotPdf | Read::Binary(_) | Read::TooLarge(_) => 0,
            };
            self.pdfs.read = Some((pair.clone(), read));
        }
        if let Some(comparison) = comparison {
            self.compared(Compared { pair, comparison });
        }
        self.rebuild_in_place();
    }

    /// The page pair the navigator selects, to render and compare.
    pub(super) fn selected_page(&self) -> Option<Page> {
        let pair = self.pdfs.pages()?.pairs.get(self.pdfs.page)?;
        Some(Page {
            base: pair.base,
            head: pair.head,
        })
    }

    /// Whether approving now approves a whole PDF: the selected file is a PDF whose pages
    /// were read, and in the diff its selected hunk is the PDF's unit, not a rename's.
    #[must_use]
    pub fn approves_pdf(&self) -> bool {
        if self.folder.is_some() || self.pdfs.pages().is_none() {
            return false;
        }
        if self.mode == super::Mode::Tree {
            return true;
        }
        let view = self
            .snapshot
            .as_ref()
            .and_then(crate::state::Snapshot::view);
        let unit = self.rows.hunks.get(self.hunk).map(|hunk| hunk.unit);
        view.zip(unit).is_some_and(|(view, unit)| {
            matches!(
                view.unit(unit).map(|record| &record.kind),
                Ok(UnitKind::File(
                    FileUnit::Binary | FileUnit::NotUtf8 | FileUnit::Oversized
                ))
            )
        })
    }

    /// What the navigator says of the selected page pair, such as `Base 2/8 → Head 4/9 ·
    /// reordered`.
    #[must_use]
    pub fn page_navigator(&self) -> Option<String> {
        self.pdfs.pages()?.describe(self.pdfs.page)
    }

    /// Whether the selected file is a PDF whose pages were read.
    #[must_use]
    pub fn shows_pdf(&self) -> bool {
        self.folder.is_none() && self.pdfs.pages().is_some()
    }

    /// What the selected PDF's unit shows, for its rows.
    pub(super) fn pdf_rows(&self, path: &str) -> Option<PdfRows> {
        let wanted = self
            .pdfs
            .wanted
            .as_ref()
            .filter(|_| self.folder.is_none())?;
        let shows = match self.read_pdf(wanted)? {
            Read::NotPdf => return None,
            Read::Binary(why) | Read::TooLarge(why) => PdfShows::Binary(why.clone()),
            Read::Pages(pages) => {
                let mut navigator = pages.describe(self.pdfs.page)?;
                if let Some(keys) = self.page_keys() {
                    write!(navigator, " · {keys}").ok();
                }
                PdfShows::Page {
                    navigator,
                    rows: pages.pairs.get(self.pdfs.page)?.rows.clone(),
                }
            }
        };
        Some(PdfRows {
            path: path.to_owned(),
            shows,
        })
    }

    /// The keys that step through the pages and show or hide them, such as `} next page ·
    /// { previous · P shows the pages`; the last only where the front end shows images.
    fn page_keys(&self) -> Option<String> {
        let key = |action: &str| {
            self.settings
                .keymap
                .iter()
                .find(|binding| {
                    binding.action == action && binding.area == crate::keymap::Area::Diff
                })
                .and_then(|binding| binding.keys.first())
                .map(|key| crate::keymap::display(key))
        };
        let mut keys = format!(
            "{} next page · {} previous",
            key("NextPage")?,
            key("PreviousPage")?
        );
        if let Some(toggle) = key("TogglePages").filter(|_| self.shows_images()) {
            let what = if self.pages_shown() { "hides" } else { "shows" };
            write!(keys, " · {toggle} {what} the pages").ok();
        }
        Some(keys)
    }

    /// `P`, `}` or `{`: what `action`, one of them, does to a changed PDF's pages.
    pub(super) fn page_key(&mut self, action: super::Action) {
        match action {
            super::Action::NextPage => self.step_page(true),
            super::Action::PreviousPage => self.step_page(false),
            _ => self.toggle_pages(),
        }
    }

    /// `P`: shows or hides a changed PDF's rendered page pair, for every PDF from now on, or
    /// says why there is none.
    fn toggle_pages(&mut self) {
        if !self.shows_pdf() {
            self.set_status("Only a changed PDF has pages to show.");
        } else if !self.shows_images() {
            self.set_status("This terminal shows no images, so a PDF shows its text alone.");
        } else {
            self.flip_pages();
            self.rebuild();
        }
    }

    /// `}` and `{`: shows the next or previous page pair of the selected PDF, from the top of
    /// its rows, since the rows of the page before say nothing of this one.
    fn step_page(&mut self, forward: bool) {
        let Some(count) = self.pdfs.pages().map(|pages| pages.pairs.len()) else {
            self.set_status("Only a changed PDF has pages to step through.");
            return;
        };
        let next = if forward {
            Some(self.pdfs.page + 1).filter(|&next| next < count)
        } else {
            self.pdfs.page.checked_sub(1)
        };
        let Some(next) = next else {
            self.set_status(if forward {
                "This is the last page."
            } else {
                "This is the first page."
            });
            return;
        };
        self.pdfs.page = next;
        self.selection = None;
        self.rebuild();
        self.diff_scroll.stop();
        self.diff_scroll.jump(0.0);
    }
}

/// Reads `pair`'s versions and, when either is a PDF, each one's page text, and otherwise,
/// when asked to `decode`, compares them as images; runs in the background.
#[tracing::instrument(skip_all)]
fn read_pages(review: &Review, pair: &Pair, decode: bool) -> (Read, Option<Comparison>) {
    let (base, head) = read(review, pair);
    let pdf = [&base, &head]
        .iter()
        .any(|version| matches!(version, Encoded::Bytes(bytes) if is_pdf(bytes)));
    if !pdf {
        let large = [(&base, "base"), (&head, "head")]
            .into_iter()
            .find_map(|(version, side)| match version {
                Encoded::TooLarge(size) => Some(Fallback::TooLarge(*size).reason(side)),
                _ => None,
            });
        let comparison = decode.then(|| Comparison::of(base, head));
        return (large.map_or(Read::NotPdf, Read::TooLarge), comparison);
    }
    let texts = |version: Encoded| match version {
        Encoded::Absent => Ok(None),
        Encoded::Bytes(bytes) => page_texts(&bytes).map(Some),
        Encoded::TooLarge(size) => Err(Fallback::TooLarge(size)),
        Encoded::Unreadable => Err(Fallback::Unreadable),
    };
    let read = match (texts(base), texts(head)) {
        (Ok(base), Ok(head)) => Read::Pages(Arc::new(Pages::of(base.as_deref(), head.as_deref()))),
        (Err(fallback), _) => Read::Binary(fallback.reason("base")),
        (_, Err(fallback)) => Read::Binary(fallback.reason("head")),
    };
    (read, None)
}
