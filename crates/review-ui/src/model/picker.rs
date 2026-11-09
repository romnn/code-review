//! The pickers, telescope's two most used: `Ctrl+P` finds a file by a fuzzy match of its path,
//! and `Ctrl+G` searches file contents as you type, with a preview of the lines around each
//! hit. Both start on the review's changed files; Tab widens them to the whole head tree.
//!
//! Searching changed files reads the head texts the window already caches for the diff, the
//! same text the diff and the buffer show, so a hit's line is exactly the line Enter opens and
//! typing starts no process. The whole tree is not in memory, so there `git grep` searches the
//! pinned head snapshot and `git ls-tree` lists it, once per picker.
//! Every search runs as a background job; a newer query cancels the older one, and only the
//! newest result lands.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use review_core::tree::NodeKind;
use review_core::{FileTone, ObjectId, Review};

use super::effects::{Outcome, Subject};
use super::{Mode, Model, Overlay, buffer};
use crate::frame::{ListScroll, Request};
use crate::rows::FullText;
use crate::state::Progress;
use crate::syntax::SideSyntax;
use crate::text::TextPos;

/// How many entries a search keeps; the list says when there were more.
const LIMIT: usize = 2000;

/// How long the content search waits for typing to pause before it searches.
const DEBOUNCE: Duration = Duration::from_millis(80);

/// The longest line text an entry keeps, in bytes.
const ENTRY_TEXT: usize = 240;

/// What a picker finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Files by a fuzzy match of their path.
    Files,
    /// Lines that contain the query.
    Content,
}

impl Kind {
    /// How long a search waits for typing to pause: the file finder matches in memory at
    /// once, the content search waits.
    #[must_use]
    pub(crate) fn debounce(self) -> Duration {
        match self {
            Self::Files => Duration::ZERO,
            Self::Content => DEBOUNCE,
        }
    }
}

/// Which files a picker covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The review's changed files.
    Changed,
    /// Every file of the head snapshot.
    Tree,
}

/// How a changed file shows in a picker: its change and its review progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    /// The change's letter, as the tree shows it, or a space for a file without one.
    pub letter: char,
    /// The tone the letter is drawn in.
    pub tone: FileTone,
    /// How far the file's review is.
    pub progress: Progress,
    /// How many of the file's review units are done: reviewed, accepted, flagged or approved
    /// automatically, as the tree counts them.
    pub reviewed: usize,
    /// How many review units the file has.
    pub total: usize,
}

/// A changed file a picker offers, with its head text when it has one.
#[derive(Clone)]
pub(crate) struct Candidate {
    /// The display path, as the tree shows it.
    pub(crate) path: String,
    pub(crate) change: Change,
    pub(crate) text: Option<Arc<FullText>>,
}

/// One entry of a picker's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file's display path.
    pub path: String,
    /// For a content hit, its zero-based line.
    pub line: Option<usize>,
    /// For a content hit, the byte column of its first match in the full line.
    pub(crate) column: usize,
    /// For a content hit, the line's text without its indentation.
    pub text: String,
    /// The bytes to highlight: of the path for a file, of `text` for a content hit.
    pub highlights: Vec<Range<usize>>,
    /// For a changed file, how it changed and how far its review is.
    pub change: Option<Change>,
}

/// The head snapshot's files, listed once when a picker first needs the whole tree.
pub(crate) struct Listing {
    pub(crate) paths: Vec<String>,
    pub(crate) oids: HashMap<String, ObjectId>,
}

/// An open picker.
pub struct Picker {
    /// Whether the picker finds files or content.
    pub kind: Kind,
    /// Which files the picker searches.
    pub scope: Scope,
    /// The query as typed.
    pub query: String,
    /// Whether the query takes typing; otherwise the list takes the keys, as vim's normal mode.
    pub editing: bool,
    /// The entries the query found, in the order the list shows them.
    pub entries: Arc<Vec<Entry>>,
    /// The selected entry's index in `entries`.
    pub selected: usize,
    /// Where the entries are scrolled.
    pub(crate) scroll: ListScroll,
    /// Whether the search stopped at `LIMIT` entries.
    pub truncated: bool,
    /// Whether a search for the query is still running.
    pub searching: bool,
    /// Why the last search failed, if it did.
    pub error: Option<String>,
    /// The revision the picker searches; a result for another one is dropped.
    revision: review_core::RevisionNumber,
    head: ObjectId,
    /// This picker, among every picker the window opened, so an answer for a closed one is
    /// never applied to a newer one; also its query field's identity.
    pub(super) id: u64,
    /// The search in flight, numbered from the same window-wide count as `id`.
    generation: u64,
    cancel: Arc<AtomicBool>,
    changed: Arc<Vec<Candidate>>,
    listing: Option<Arc<Listing>>,
    /// Texts of unchanged files, read for the preview.
    texts: HashMap<String, Arc<FullText>>,
    /// Files whose text could not be read (binary or failing), so they are not read again.
    pub unreadable: HashSet<String>,
    reading: Option<String>,
    /// Enter came while a search ran: the entry opens once the newest result lands.
    open_when_found: bool,
}

/// What one search needs, moved to the background.
pub(super) struct Query {
    /// The search's generation, which its result names.
    generation: u64,
    kind: Kind,
    scope: Scope,
    text: String,
    review: Arc<Review>,
    head: ObjectId,
    changed: Arc<Vec<Candidate>>,
    listing: Option<Arc<Listing>>,
    cancel: Arc<AtomicBool>,
}

/// What one search found.
pub(super) struct Found {
    pub(super) entries: Vec<Entry>,
    pub(super) truncated: bool,
    pub(super) listing: Option<Arc<Listing>>,
    pub(super) error: Option<String>,
}

impl Picker {
    /// Whether a search or a preview read is running.
    pub(crate) fn busy(&self) -> bool {
        self.searching || self.reading.is_some()
    }

    /// The selected entry.
    #[must_use]
    pub fn entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// The head text of `path`, when it is in memory.
    #[must_use]
    pub fn text(&self, path: &str) -> Option<Arc<FullText>> {
        self.changed
            .iter()
            .find(|candidate| candidate.path == path)
            .and_then(|candidate| candidate.text.clone())
            .or_else(|| self.texts.get(path).cloned())
    }
}

impl Model {
    /// `Ctrl+P` and `Ctrl+G`: opens the file finder or the content search.
    pub(super) fn open_picker(&mut self, kind: Kind) {
        let (Some(snapshot), Some(record)) = (self.snapshot.as_ref(), self.revision_record())
        else {
            self.set_status("The review is not open yet.");
            return;
        };
        let revision = snapshot.revision;
        // One overlay owns the keys at a time.
        if self.settings_panel.is_some() {
            self.close_settings();
        }
        let changed = Arc::new(self.candidates());
        self.picker_runs += 1;
        self.picker = Some(Picker {
            kind,
            scope: Scope::Changed,
            query: String::new(),
            editing: true,
            entries: Arc::new(Vec::new()),
            selected: 0,
            scroll: ListScroll::default(),
            truncated: false,
            searching: false,
            error: None,
            revision,
            head: record.head,
            id: self.picker_runs,
            generation: self.picker_runs,
            cancel: Arc::new(AtomicBool::new(false)),
            changed,
            listing: None,
            texts: HashMap::new(),
            unreadable: HashSet::new(),
            reading: None,
            open_when_found: false,
        });
        self.overlay = Some(Overlay::Picker);
        self.selection = None;
        self.run_picker(Duration::ZERO);
    }

    /// The shown revision's record.
    fn revision_record(&self) -> Option<review_core::RevisionRecord> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot
            .history
            .revisions()
            .into_iter()
            .find(|record| record.number == snapshot.revision)
            .cloned()
    }

    /// The changed files in tree order, with their change, progress and head text.
    fn candidates(&self) -> Vec<Candidate> {
        let Some(view) = self.view() else {
            return Vec::new();
        };
        // Each changed file's head blob by display path, in one pass over the analysis.
        let heads: HashMap<&str, &ObjectId> = view
            .analysis()
            .map(|analysis| {
                analysis
                    .files
                    .iter()
                    .filter_map(|file| Some((file.display_path(), &file.new.as_ref()?.oid)))
                    .collect()
            })
            .unwrap_or_default();
        self.tree
            .iter()
            .filter(|node| node.kind == NodeKind::File)
            .map(|node| {
                let head = heads.get(node.path.as_str());
                let progress = Progress::of(&node.counts);
                Candidate {
                    path: node.path.clone(),
                    change: Change {
                        letter: node
                            .change
                            .as_ref()
                            .map_or(' ', |change| change.kind.letter()),
                        tone: node
                            .change
                            .as_ref()
                            .map_or(node.tone, review_core::FileChangeSummary::tone),
                        progress,
                        reviewed: node.counts.done(),
                        total: node.counts.units,
                    },
                    text: head.and_then(|oid| self.blobs.texts.get(*oid).cloned()),
                }
            })
            .collect()
    }

    /// Searches for the picker's query after `delay`, cancelling the search before it.
    pub(super) fn run_picker(&mut self, delay: Duration) {
        let Some(review) = self.review.clone() else {
            return;
        };
        self.picker_runs += 1;
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.cancel.store(true, Ordering::Relaxed);
        picker.cancel = Arc::new(AtomicBool::new(false));
        picker.generation = self.picker_runs;
        picker.searching = true;
        let generation = picker.generation;
        let query = Query {
            generation,
            kind: picker.kind,
            scope: picker.scope,
            text: picker.query.clone(),
            review,
            head: picker.head.clone(),
            changed: Arc::clone(&picker.changed),
            listing: picker.listing.clone(),
            cancel: Arc::clone(&picker.cancel),
        };
        if delay.is_zero() {
            self.search_picker(query);
        } else {
            // A newer query, or a picker closed meanwhile, drops this search unstarted.
            self.after(delay, Subject::Search(generation), Outcome::Paused(query));
        }
    }

    /// The generation of the open picker's newest search.
    pub(super) fn picker_generation(&self) -> Option<u64> {
        self.picker.as_ref().map(|picker| picker.generation)
    }

    /// Runs the search `query` in the background; only the newest search's result lands.
    pub(super) fn search_picker(&mut self, query: Query) {
        self.spawn(Subject::Search(query.generation), move || {
            Outcome::Found(search(&query))
        });
    }

    /// Takes in what the open picker's newest search found.
    pub(super) fn picker_found(&mut self, found: Found) {
        let mut open = false;
        if let Some(picker) = &mut self.picker {
            picker.entries = Arc::new(found.entries);
            picker.truncated = found.truncated;
            picker.error = found.error;
            picker.searching = false;
            if found.listing.is_some() {
                picker.listing = found.listing;
            }
            picker.selected = 0;
            picker.scroll.request(Request::Top(0));
            open = std::mem::take(&mut picker.open_when_found);
        }
        if open {
            self.picker_open();
        } else {
            self.read_preview();
        }
    }

    /// Reads, in the background, the text of the selected entry's file when the preview needs
    /// it and it is not in memory.
    fn read_preview(&mut self) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let Some(picker) = &mut self.picker else {
            return;
        };
        if picker.kind != Kind::Content || picker.reading.is_some() {
            return;
        }
        let Some(path) = picker.entry().map(|entry| entry.path.clone()) else {
            return;
        };
        if picker.text(&path).is_some() || picker.unreadable.contains(&path) {
            return;
        }
        let Some(oid) = picker
            .listing
            .as_ref()
            .and_then(|listing| listing.oids.get(&path).cloned())
        else {
            return;
        };
        picker.reading = Some(path.clone());
        let id = picker.id;
        self.spawn(Subject::Picker(id), move || {
            let text = read_text(&review, &oid);
            Outcome::PreviewText(PreviewText { path, text })
        });
    }

    /// Takes in the text of a file the open picker's preview needs.
    pub(super) fn preview_read(&mut self, read: PreviewText) {
        let PreviewText { path, text } = read;
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.reading = None;
        match text {
            Some(text) => {
                picker.texts.insert(path, Arc::new(text));
            }
            None => {
                picker.unreadable.insert(path);
            }
        }
        // The selection may have moved on while this file was read.
        self.read_preview();
    }

    pub(super) fn picker_step(&mut self, forward: bool) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let count = picker.entries.len();
        if count == 0 {
            return;
        }
        picker.selected = if forward {
            (picker.selected + 1) % count
        } else {
            (picker.selected + count - 1) % count
        };
        picker.scroll.request(Request::Nearest(picker.selected));
        self.read_preview();
    }

    /// A click on the picker's entry `index`: selects it, and a double click opens it.
    pub fn click_picker_entry(&mut self, index: usize, clicks: usize) {
        if let Some(picker) = &mut self.picker {
            picker.selected = index;
        }
        if clicks > 1 {
            self.picker_open();
        } else {
            self.read_preview();
        }
    }

    /// Tab: between the changed files and the whole tree.
    pub(super) fn picker_scope(&mut self) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.scope = match picker.scope {
            Scope::Changed => Scope::Tree,
            Scope::Tree => Scope::Changed,
        };
        self.run_picker(Duration::ZERO);
    }

    /// `i` or `/` in the list gives the query the keys (`true`); Esc while typing gives them
    /// to the list.
    pub(super) fn picker_typing(&mut self, editing: bool) {
        if let Some(picker) = &mut self.picker {
            picker.editing = editing;
        }
    }

    pub(super) fn close_picker(&mut self) {
        if let Some(picker) = self.picker.take() {
            picker.cancel.store(true, Ordering::Relaxed);
        }
        self.overlay = None;
    }

    /// Enter: opens the selected entry. A changed file opens in the diff, at the hunk that holds
    /// a content hit, or in the buffer at the hit's line when no hunk shows it; a file outside
    /// the review opens read-only in the buffer.
    pub(super) fn picker_open(&mut self) {
        let Some(picker) = &self.picker else {
            return;
        };
        // The entries belong to the query before the one being searched; the newest result
        // opens once it lands.
        if picker.searching {
            if let Some(picker) = &mut self.picker {
                picker.open_when_found = true;
            }
            return;
        }
        let Some(entry) = picker.entry().cloned() else {
            return;
        };
        let current = self.snapshot.as_ref().map(|snapshot| snapshot.revision);
        let oid = picker
            .listing
            .as_ref()
            .and_then(|listing| listing.oids.get(&entry.path).cloned());
        if current != Some(picker.revision) {
            self.close_picker();
            self.set_status("The review moved to another revision; search again.");
            return;
        }
        // Everything an entry needs is checked before the picker closes.
        let column = entry.column;
        if entry.change.is_some() {
            self.close_picker();
            self.open_changed(&entry.path, entry.line, column);
        } else if let Some(oid) = oid {
            self.close_picker();
            self.open_unchanged(entry.path, oid, entry.line.unwrap_or(0), column);
        } else {
            self.fail(format!(
                "{} is not in the head snapshot's listing.",
                entry.path
            ));
        }
    }

    /// Shows a changed file's diff, at the hunk that changed head line `line` when one did,
    /// else opens it as a buffer at that line.
    fn open_changed(&mut self, path: &str, line: Option<usize>, column: usize) {
        self.show_file(path);
        self.set_status(match line {
            Some(line) => format!("{path}:{}", line + 1),
            None => path.to_owned(),
        });
        let Some(line) = line else {
            self.enter_mode(Mode::Diff);
            self.reveal_hunk();
            return;
        };
        // The hunk whose units changed that head line, even when it is folded.
        let hunk = self.view().and_then(|view| {
            self.rows.hunks.iter().position(|units| {
                units.iter().any(|&unit| {
                    view.unit(unit)
                        .ok()
                        .and_then(|record| record.chunk())
                        .is_some_and(|chunk| chunk.new.range().contains(&line))
                })
            })
        });
        let row = self.rows.rows.iter().position(|row| {
            row.cells()
                .any(|cell| cell.head.as_ref().is_some_and(|head| head.line == line))
        });
        if let Some(hunk) = hunk {
            self.enter_mode(Mode::Diff);
            self.select_hunk(hunk);
            if let Some(row) = row.filter(|row| {
                self.hunk_rows
                    .get(hunk)
                    .is_some_and(|rows| rows.contains(row))
            }) {
                self.reveal_match(row);
            }
        } else {
            self.enter_buffer();
            let opened = self.mode == Mode::Buffer
                && self
                    .buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.origin == buffer::Origin::Review(path.to_owned()));
            if opened {
                self.set_buffer_cursor(line, column);
            } else {
                // The file cannot show in full (binary, or not read): its diff shows instead,
                // never a buffer of another file.
                self.enter_mode(Mode::Diff);
                self.reveal_hunk();
                self.set_status(format!(
                    "{path}:{} is outside the diff, and the file cannot show in full.",
                    line + 1
                ));
            }
        }
    }

    /// Opens a file the review did not change as a read-only buffer at `line`, reading and
    /// highlighting it in the background.
    fn open_unchanged(&mut self, path: String, oid: ObjectId, line: usize, column: usize) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let Some(revision) = self.snapshot.as_ref().map(|snapshot| snapshot.revision) else {
            return;
        };
        self.set_status(format!("Opening {path}…"));
        self.highlighting += 1;
        self.spawn(Subject::Revision(revision), move || {
            let text = read_text(&review, &oid);
            let syntax = text.as_ref().and_then(|text| {
                review_core::syntax::Language::from_path(&path)
                    .map(|_| Arc::new(crate::syntax::analyze(&path, &text.lines.join("\n"))))
            });
            Outcome::Unchanged(Unchanged {
                path,
                line,
                column,
                text,
                syntax,
            })
        });
    }

    /// Opens a file the review did not change, read for the revision that shows, as a
    /// read-only buffer.
    pub(super) fn unchanged_read(&mut self, unchanged: Unchanged) {
        let Unchanged {
            path,
            line,
            column,
            text,
            syntax,
        } = unchanged;
        let Some(text) = text else {
            self.fail(format!("{path} is binary or could not be read."));
            return;
        };
        let buffer = buffer::Buffer {
            id: self.next_buffer_id(),
            origin: buffer::Origin::Head(path.clone()),
            lines: Arc::new(text),
            syntax,
            markers: Arc::new(Vec::new()),
            cursor: TextPos { row: 0, col: 0 },
            previous: None,
        };
        self.set_buffer(Some(buffer));
        self.buffer_scroll.stop();
        self.enter_mode(Mode::Buffer);
        self.set_buffer_cursor(line, column);
        self.set_status(format!("{path}:{} · not changed in this review", line + 1));
    }
}

/// The text of a file that the content search's preview needs, `None` for a binary file or a
/// failed read.
pub(super) struct PreviewText {
    pub(super) path: String,
    pub(super) text: Option<FullText>,
}

/// A file the review did not change, read to open as a buffer at a line and column.
pub(super) struct Unchanged {
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) column: usize,
    /// Its text, `None` for a binary file or a failed read.
    pub(super) text: Option<FullText>,
    /// Its highlights, when its language has them.
    pub(super) syntax: Option<Arc<SideSyntax>>,
}

/// Reads blob `oid` as text; `None` for a binary blob or a failed read.
fn read_text(review: &Review, oid: &ObjectId) -> Option<FullText> {
    let mut reader = review.repo().blob_reader().ok()?;
    match reader.read(oid).ok()? {
        review_core::git::Blob::Content(bytes) if !bytes.contains(&0) => {
            Some(FullText::new(&String::from_utf8_lossy(&bytes)))
        }
        _ => None,
    }
}

/// Runs one search.
fn search(query: &Query) -> Found {
    let mut found = Found {
        entries: Vec::new(),
        truncated: false,
        listing: None,
        error: None,
    };
    let mut listing = query.listing.clone();
    if query.scope == Scope::Tree && listing.is_none() {
        match query.review.repo().tree_files(&query.head) {
            Ok(files) => {
                let loaded = Arc::new(Listing {
                    oids: files
                        .iter()
                        .map(|file| (file.path.clone(), file.oid.clone()))
                        .collect(),
                    paths: files.into_iter().map(|file| file.path).collect(),
                });
                found.listing = Some(Arc::clone(&loaded));
                listing = Some(loaded);
            }
            Err(err) => found.error = Some(format!("listing the tree failed: {err}")),
        }
    }
    let changes: HashMap<&str, Change> = query
        .changed
        .iter()
        .map(|candidate| (candidate.path.as_str(), candidate.change))
        .collect();
    match query.kind {
        Kind::Files => {
            let mut candidates: Vec<(&str, Option<Change>)> = query
                .changed
                .iter()
                .map(|candidate| (candidate.path.as_str(), Some(candidate.change)))
                .collect();
            if let Some(listing) = listing.as_ref().filter(|_| query.scope == Scope::Tree) {
                candidates.extend(
                    listing
                        .paths
                        .iter()
                        .filter(|path| !changes.contains_key(path.as_str()))
                        .map(|path| (path.as_str(), None)),
                );
            }
            let (entries, truncated) = match_files(&candidates, &query.text, LIMIT);
            found.entries = entries;
            found.truncated = truncated;
        }
        Kind::Content if query.text.is_empty() => {}
        Kind::Content => {
            let ignore_case = !query.text.chars().any(char::is_uppercase);
            match query.scope {
                Scope::Changed => {
                    let files: Vec<(&str, Change, &FullText)> = query
                        .changed
                        .iter()
                        .filter_map(|candidate| {
                            Some((
                                candidate.path.as_str(),
                                candidate.change,
                                candidate.text.as_deref()?,
                            ))
                        })
                        .collect();
                    let (entries, truncated) =
                        grep_texts(&files, &query.text, ignore_case, LIMIT, &query.cancel);
                    found.entries = entries;
                    found.truncated = truncated;
                }
                Scope::Tree => match query.review.repo().grep(
                    &query.head,
                    &query.text,
                    ignore_case,
                    LIMIT + 1,
                    &query.cancel,
                ) {
                    Ok(lines) => {
                        found.truncated = lines.len() > LIMIT;
                        found.entries = lines
                            .into_iter()
                            .take(LIMIT)
                            .map(|hit| {
                                let change = changes.get(hit.path.as_str()).copied();
                                hit_entry(
                                    hit.path,
                                    hit.line,
                                    &hit.text,
                                    &query.text,
                                    ignore_case,
                                    change,
                                )
                            })
                            .collect();
                    }
                    Err(err) => found.error = Some(format!("git grep failed: {err}")),
                },
            }
        }
    }
    found
}

/// Fuzzy-matches `query` against the candidates' paths, best first; ties keep changed files
/// first, then the candidates' order. An empty query keeps every candidate in order.
#[must_use]
pub(crate) fn match_files(
    candidates: &[(&str, Option<Change>)],
    query: &str,
    limit: usize,
) -> (Vec<Entry>, bool) {
    let entry = |path: &str, change: Option<Change>, highlights: Vec<Range<usize>>| Entry {
        path: path.to_owned(),
        line: None,
        column: 0,
        text: String::new(),
        highlights,
        change,
    };
    if query.trim().is_empty() {
        let entries = candidates
            .iter()
            .take(limit)
            .map(|&(path, change)| entry(path, change, Vec::new()))
            .collect();
        return (entries, candidates.len() > limit);
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut chars = Vec::new();
    let mut indices = Vec::new();
    let mut scored: Vec<(u32, usize, Vec<Range<usize>>)> = Vec::new();
    for (position, &(path, _)) in candidates.iter().enumerate() {
        indices.clear();
        let Some(score) =
            pattern.indices(Utf32Str::new(path, &mut chars), &mut matcher, &mut indices)
        else {
            continue;
        };
        indices.sort_unstable();
        indices.dedup();
        scored.push((score, position, char_ranges(path, &indices)));
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| {
                let changed = |position: usize| {
                    candidates
                        .get(position)
                        .is_some_and(|(_, change)| change.is_some())
                };
                changed(b.1).cmp(&changed(a.1))
            })
            .then_with(|| a.1.cmp(&b.1))
    });
    let truncated = scored.len() > limit;
    let entries = scored
        .into_iter()
        .take(limit)
        .filter_map(|(_, position, highlights)| {
            let &(path, change) = candidates.get(position)?;
            Some(entry(path, change, highlights))
        })
        .collect();
    (entries, truncated)
}

/// The byte ranges of `text` covering the characters at `indices` (sorted), merged where they
/// touch.
fn char_ranges(text: &str, indices: &[u32]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut wanted = indices.iter().peekable();
    for (position, (start, character)) in text.char_indices().enumerate() {
        let Some(&&next) = wanted.peek() else {
            break;
        };
        if usize::try_from(next).ok() != Some(position) {
            continue;
        }
        wanted.next();
        let end = start + character.len_utf8();
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

/// Every non-overlapping occurrence of `needle` in `text`, ignoring ASCII case when asked.
#[must_use]
pub fn occurrences(text: &str, needle: &str, ignore_case: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    if !ignore_case {
        return text
            .match_indices(needle)
            .map(|(start, found)| start..start + found.len())
            .collect();
    }
    crate::find::ranges(text, needle)
}

/// An entry for a hit on zero-based line `line` of `path`, whose text is `text`.
fn hit_entry(
    path: String,
    line: usize,
    text: &str,
    needle: &str,
    ignore_case: bool,
    change: Option<Change>,
) -> Entry {
    let indent = text.len() - text.trim_start().len();
    let mut end = text.len().min(indent + ENTRY_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let shown = text.get(indent..end).unwrap_or_default().to_owned();
    let highlights = occurrences(&shown, needle, ignore_case);
    let column = occurrences(text, needle, ignore_case)
        .first()
        .map_or(0, |found| found.start);
    Entry {
        path,
        line: Some(line),
        column,
        text: shown,
        highlights,
        change,
    }
}

/// Every line of `files` that contains `needle`, in file and line order, up to `limit`; the
/// search stops early once `cancel` is set.
#[must_use]
pub(crate) fn grep_texts(
    files: &[(&str, Change, &FullText)],
    needle: &str,
    ignore_case: bool,
    limit: usize,
    cancel: &AtomicBool,
) -> (Vec<Entry>, bool) {
    let mut entries = Vec::new();
    let lowered = needle.to_ascii_lowercase();
    for &(path, change, text) in files {
        if cancel.load(Ordering::Relaxed) {
            return (Vec::new(), false);
        }
        for (line, content) in text.lines.iter().enumerate() {
            let hit = if ignore_case {
                content.to_ascii_lowercase().contains(&lowered)
            } else {
                content.contains(needle)
            };
            if !hit {
                continue;
            }
            if entries.len() == limit {
                return (entries, true);
            }
            entries.push(hit_entry(
                path.to_owned(),
                line,
                content,
                needle,
                ignore_case,
                Some(change),
            ));
        }
    }
    (entries, false)
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    /// A content hit as its line, its column, its text and its highlighted spans.
    type Hit<'a> = (usize, usize, &'a str, Vec<(usize, usize)>);

    /// Ranges as (start, end) pairs, which compare more readably.
    fn spans(ranges: &[Range<usize>]) -> Vec<(usize, usize)> {
        ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect()
    }

    fn change() -> Change {
        Change {
            letter: 'M',
            tone: FileTone::Modified,
            progress: Progress::Open,
            reviewed: 0,
            total: 1,
        }
    }

    /// The best fuzzy match comes first, with its matched characters highlighted, and a
    /// changed file wins a tie.
    #[test]
    fn files_match_fuzzily_best_first() {
        let candidates = [
            ("src/app/render.rs", None),
            ("docs/readme.md", None),
            ("src/rows.rs", Some(change())),
        ];
        let (entries, truncated) = match_files(&candidates, "rows", 10);
        assert!(!truncated);
        assert_eq!(
            entries.first().map(|entry| entry.path.as_str()),
            Some("src/rows.rs")
        );
        assert_eq!(
            entries.first().map(|entry| spans(&entry.highlights)),
            Some(vec![(4, 8)])
        );
        let (all, _) = match_files(&candidates, "", 2);
        assert_eq!(
            all.len(),
            2,
            "an empty query lists the candidates up to the limit"
        );
        let (none, _) = match_files(&candidates, "zzz", 10);
        assert_eq!(none, Vec::new());
    }

    /// Content hits are whole lines, trimmed of their indentation with the match moved along,
    /// in file order, and cut at the limit.
    #[test]
    fn content_hits_trim_and_highlight() {
        let text = FullText::new(indoc! {"
            fn main() {
                let needle = 1;
            }
            NEEDLE
        "});
        let cancel = AtomicBool::new(false);
        let files = [("a.rs", change(), &text)];
        let (entries, truncated) = grep_texts(&files, "needle", true, 10, &cancel);
        assert!(!truncated);
        let found: Vec<Hit<'_>> = entries
            .iter()
            .map(|entry| {
                (
                    entry.line.unwrap(),
                    entry.column,
                    entry.text.as_str(),
                    spans(&entry.highlights),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                (1, 8, "let needle = 1;", vec![(4, 10)]),
                (3, 0, "NEEDLE", vec![(0, 6)])
            ],
            "the column is the match's in the full line, before trimming"
        );
        let (exact, _) = grep_texts(&files, "NEEDLE", false, 10, &cancel);
        assert_eq!(exact.len(), 1, "an upper-case query matches case");
        let (cut, truncated) = grep_texts(&files, "needle", true, 1, &cancel);
        assert_eq!((cut.len(), truncated), (1, true));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            grep_texts(&files, "needle", true, 10, &cancel).0,
            Vec::new()
        );
    }

    #[test]
    fn character_indices_become_byte_ranges() {
        assert_eq!(spans(&char_ranges("aébc", &[1, 2, 3])), [(1, 5)]);
        assert_eq!(spans(&char_ranges("abc", &[0, 2])), [(0, 1), (2, 3)]);
    }
}
