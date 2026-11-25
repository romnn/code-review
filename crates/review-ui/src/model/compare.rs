//! The comparison of the selected file's versions when they are images: which pair of blobs
//! the selected file shows, comparing it in the background, and whether the changed pixels
//! show over the head.
//!
//! A file is compared when its content changed as one opaque unit (binary, not UTF-8 or too
//! large to diff) between regular files; it shows a comparison once either version turns out
//! to be an image, and otherwise stays the binary file it was.
//! Review state is untouched: the opaque unit is approved as any unit is, and any change of
//! either blob reopens it, whatever the pixels.
//!
//! Nothing is read or decoded while the front end shows no images, as a terminal without an
//! image protocol does, where the file stays the binary file it was.
//!
//! From the moment a file that may be an image is selected, its comparison is pending, so the
//! front ends lay out its place at once and the rows below never move when it lands; it says
//! it is decoding only once that takes longer than [`DECODING`], so a quick one never flashes.
//!
//! One job compares at a time, and one comparison is kept, for the pair compared last, failures
//! included.
//! A comparison lands only while its pair is still the one the selected file shows, so a late
//! one never shows under another file, and the pair shown now starts once the job before it
//! ends.

use std::time::Duration;

use review_core::analysis::{FileUnit, UnitKind};
use review_core::git::Blob;
use review_core::{ObjectId, Review, RevisionView};

use super::Model;
use super::effects::{Because, Outcome, Subject};
use super::sync::Tick;
use crate::images::{Changes, Comparison, Encoded, MAX_BYTES};

/// The blobs of a changed file's base and head, in that order; either is absent for an
/// addition or a deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    /// The base's blob.
    pub base: Option<ObjectId>,
    /// The head's blob.
    pub head: Option<ObjectId>,
}

/// How long a comparison may take before its place says it is decoding.
const DECODING: Duration = Duration::from_millis(100);

/// A comparison the selected file waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    /// Whether it has taken longer than [`DECODING`], so its place says it is decoding.
    pub slow: bool,
}

/// A pair, with what comparing its versions found.
#[derive(Debug)]
pub struct Compared {
    /// The pair compared.
    pub pair: Pair,
    /// What comparing it found.
    pub comparison: Comparison,
}

/// The selected file's comparison, and the job that makes it.
#[derive(Debug)]
pub(super) struct Images {
    /// Whether the front end shows images.
    shown: bool,
    /// The pair the selected file shows, when it is compared.
    wanted: Option<Pair>,
    /// The pair compared last.
    compared: Option<Compared>,
    /// The pair a job compares now.
    comparing: Option<Pair>,
    /// Whether the changed pixels show over the head.
    changes: bool,
    /// How many times the wanted pair changed, by which a timer knows its pair still waits.
    waits: u64,
    /// Whether the wanted pair has waited longer than [`DECODING`].
    slow: bool,
}

impl Images {
    /// No comparison yet, for a front end that shows images when `shown`.
    pub(super) fn new(shown: bool) -> Self {
        Self {
            shown,
            wanted: None,
            compared: None,
            comparing: None,
            changes: false,
            waits: 0,
            slow: false,
        }
    }

    /// Whether a job compares a pair now.
    pub(super) fn busy(&self) -> bool {
        self.comparing.is_some()
    }

    /// Notes that the job comparing a pair ended, whether or not what it found lands.
    pub(super) fn ended(&mut self) {
        self.comparing = None;
    }
}

impl Model {
    /// The selected file's comparison, once one of its versions turned out to be an image.
    #[must_use]
    pub fn comparison(&self) -> Option<&Compared> {
        let compared = self.images.compared.as_ref()?;
        let shown = self.images.shown && self.images.wanted.as_ref() == Some(&compared.pair);
        (shown && compared.comparison.has_image()).then_some(compared)
    }

    /// The comparison the selected file waits for: a file that may be an image, while its
    /// versions are read and decoded.
    #[must_use]
    pub fn pending_comparison(&self) -> Option<Pending> {
        let wanted = self.images.wanted.as_ref().filter(|_| self.images.shown)?;
        let compared = self.images.compared.as_ref();
        (compared.map(|compared| &compared.pair) != Some(wanted)).then_some(Pending {
            slow: self.images.slow,
        })
    }

    /// Whether the front end shows images, so a changed image is compared.
    #[must_use]
    pub fn shows_images(&self) -> bool {
        self.images.shown
    }

    /// Says whether the front end shows images now, as a terminal says once it is known to:
    /// while it does not, no image is read, decoded or shown.
    pub fn show_images(&mut self, shown: bool) {
        if self.images.shown != shown {
            self.images.shown = shown;
            self.wait();
            self.compare_wanted();
        }
    }

    /// Whether the changed pixels show over the head's image, where the versions have them.
    #[must_use]
    pub fn changed_pixels_shown(&self) -> bool {
        self.images.changes
    }

    /// Follows the selected file's pair in `view`, as its rows are laid out, and starts
    /// comparing it unless it is compared or another pair is being compared.
    pub(super) fn follow_comparison(&mut self, view: &RevisionView<'_>) {
        let wanted = match (&self.folder, &self.selected) {
            (None, Some(path)) => opaque_pair(view, path),
            _ => None,
        };
        if wanted != self.images.wanted {
            self.images.wanted = wanted;
            self.wait();
        }
        self.compare_wanted();
    }

    /// Starts the wait of the pair wanted now: its place says it is decoding once
    /// [`DECODING`] has passed.
    fn wait(&mut self) {
        self.images.waits += 1;
        self.images.slow = false;
        if self.images.shown && self.images.wanted.is_some() {
            self.after(
                DECODING,
                Subject::Always(Because::Clock),
                Outcome::Tick(Tick::Decoding(self.images.waits)),
            );
        }
    }

    /// Takes in the timer of wait `waits`: while that wait lasts, its place says it is
    /// decoding.
    pub(super) fn waited_for_comparison(&mut self, waits: u64) {
        if waits == self.images.waits {
            self.images.slow = true;
        }
    }

    /// Starts comparing the pair the selected file shows, unless it is compared already or a
    /// job compares another.
    fn compare_wanted(&mut self) {
        let Some(pair) = self.images.wanted.clone().filter(|_| self.images.shown) else {
            return;
        };
        let done = self
            .images
            .compared
            .as_ref()
            .is_some_and(|compared| compared.pair == pair);
        if done || self.images.comparing.is_some() {
            return;
        }
        self.images.comparing = Some(pair.clone());
        // Keyed by blobs, whose content never changes, so it lands whatever shows meanwhile and
        // `compared` decides whether it still applies.
        self.spawn_with(Subject::Always(Because::Content), move |parts| {
            Outcome::Compared(Box::new(compare(&parts.review, pair)))
        });
    }

    /// Takes in what a job's comparison found: keeps it while its pair is the one shown, and
    /// starts the pair shown now.
    pub(super) fn compared(&mut self, compared: Compared) {
        if self.images.wanted.as_ref() == Some(&compared.pair) {
            self.images.compared = Some(compared);
        }
        self.compare_wanted();
    }

    /// `o`: shows or hides the changed pixels over the head, or says why there are none.
    pub(super) fn toggle_changed_pixels(&mut self) {
        let changes = self
            .comparison()
            .map(|compared| &compared.comparison.changes);
        let refusal = match changes {
            Some(Changes::Pixels { changed: 1.., .. }) => {
                self.images.changes = !self.images.changes;
                return;
            }
            Some(Changes::Pixels { .. }) => "The compared images show no pixel differences.",
            Some(Changes::Sizes) => "The versions differ in size, so no pixels are compared.",
            Some(Changes::None) => "Only one version is an image, so no pixels are compared.",
            None if !self.images.shown => {
                "This terminal shows no images, so a changed image shows as a binary file."
            }
            None => "Only a changed image has changed pixels to show.",
        };
        self.set_status(refusal);
    }
}

/// The pair `path` shows in `view` when its content changed as one opaque unit and each of
/// its versions is a regular file.
fn opaque_pair(view: &RevisionView<'_>, path: &str) -> Option<Pair> {
    let units = view.units_of_file(path);
    let file = view.file(*units.first()?).ok()?;
    let opaque = units.iter().any(|&unit| {
        matches!(
            view.unit(unit).map(|record| &record.kind),
            Ok(UnitKind::File(
                FileUnit::Binary | FileUnit::NotUtf8 | FileUnit::Oversized
            ))
        )
    });
    let regular = [&file.old, &file.new]
        .into_iter()
        .flatten()
        .all(super::is_regular);
    (opaque && regular).then(|| Pair {
        base: file.old.as_ref().map(|side| side.oid.clone()),
        head: file.new.as_ref().map(|side| side.oid.clone()),
    })
}

/// Reads and compares `pair`'s versions; runs in the background.
///
/// Each blob's size is looked up before it is read, since a reader streams a blob past its
/// ceiling through its pipe to skip it.
#[tracing::instrument(skip_all)]
fn compare(review: &Review, pair: Pair) -> Compared {
    let repo = review.repo();
    let mut reader = repo.blob_reader().ok();
    let mut read = |oid: Option<&ObjectId>| {
        let Some(oid) = oid else {
            return Encoded::Absent;
        };
        match repo.blob_size(oid) {
            Ok(size) if size > MAX_BYTES => Encoded::TooLarge(size),
            Ok(_) => match reader.as_mut().map(|reader| reader.read(oid)) {
                Some(Ok(Blob::Content(bytes))) => Encoded::Bytes(bytes),
                Some(Ok(Blob::Oversized { size })) => Encoded::TooLarge(size),
                Some(Err(_)) | None => Encoded::Unreadable,
            },
            Err(_) => Encoded::Unreadable,
        }
    };
    let base = read(pair.base.as_ref());
    let head = read(pair.head.as_ref());
    Compared {
        comparison: Comparison::of(base, head),
        pair,
    }
}
