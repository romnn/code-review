//! Following a working-tree review live: watching the working tree, capturing it when it
//! changes, and showing each capture in place of the snapshot on screen.
//!
//! A capture is compared with the snapshot on screen when it is taken, and lands only on that
//! snapshot: the correspondence between the two carries the reviewer's place and folds over,
//! and tells which hunks changed.
//! It waits while the reviewer is in the middle of something it would pull the rows from
//! under: marks being saved, a picker or overlay, a drag, a search being typed.
//! Only the newest waiting capture is kept, and one compared with a snapshot no longer on
//! screen, or read before a mark was saved, is taken again.
//!
//! A review key never acts on a hunk that changed under it: a mark that covers a hunk is
//! refused for a moment after the hunk changed, and for as long as a key pressed before the
//! change keeps repeating.
//! A terminal sends a held key as repeated presses, so a press is told from a held key by the
//! time between them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use review_core::correspond::{Link, LinkKind, correspond};
use review_core::{History, ObjectId, ReviewTarget, RevisionView, UnitId, UnitIndex};

use super::buffer::Origin;
use super::effects::{Because, Completion, Outcome, Subject};
use super::present::{Anchor, RowKey, diff_anchor, diff_offset};
use super::sync::Tick;
use super::{Effect, Mode, Model, Phase, is_regular};
use crate::frame;
use crate::rows::{FullText, Layout};
use crate::state::{self, Snapshot};
use crate::watch::{Waited, Waiter, Watch};

/// How often the working tree is captured whatever the watch saw, catching what it cannot see:
/// a ref moving without the index changing, a submodule's commit, events the system dropped.
const RECONCILE: Duration = Duration::from_secs(30);

/// How often the working tree is captured when it cannot be watched.
const POLL: Duration = Duration::from_secs(5);

/// How often the periodic capture walks the watched tree again first, catching a directory
/// whose events were lost without a word; a walk asks git about every directory, about half a
/// second on a checkout of 13,000 files, so it runs far less often than the capture.
const REWALK: Duration = Duration::from_secs(300);

/// How long a hunk that changed refuses review keys.
const SETTLE: Duration = Duration::from_secs(1);

/// Review keys closer together than this are one key held down.
const REPEAT: Duration = Duration::from_millis(500);

/// Following the working tree.
#[derive(Default)]
pub(crate) struct Live {
    /// The watch on the working tree, once it started; `None` while it starts or when the
    /// working tree cannot be watched, which captures it every [`POLL`] instead.
    watch: Option<Arc<Watch>>,
    /// Whether a capture runs.
    capturing: bool,
    /// Whether another capture follows the running one, and whether it walks the working
    /// tree again first.
    again: Option<bool>,
    /// The newest capture that waits for the reviewer.
    waiting: Option<Box<Captured>>,
    /// When each unit on screen last changed, for those that changed since the review opened.
    changed: HashMap<UnitId, Instant>,
    /// The review key held or pressed last.
    press: Option<Press>,
    /// Whether the watch's trouble was logged already, so it is logged once.
    troubled: bool,
    /// When the watched tree was last walked, by the watch's start or a periodic capture.
    walked: Option<Instant>,
}

impl Live {
    /// Whether a capture runs, which review-gui's QA mode waits for.
    pub(crate) fn capturing(&self) -> bool {
        self.capturing
    }
}

/// A review key pressed, and held for as long as it repeats.
#[derive(Debug, Clone, Copy)]
struct Press {
    started: Instant,
    last: Instant,
}

/// A watch that started, with its waiter.
pub(super) struct Started {
    watch: Watch,
    waiter: Waiter,
}

/// What a wait for the working tree saw, with the waiter for the next wait.
pub(super) struct WaitedFor {
    waiter: Waiter,
    waited: Waited,
}

/// A capture of the working tree, compared with the snapshot on screen when it was taken.
pub(super) struct Captured {
    /// The generation of the snapshot it was compared with; see [`Model::shown`].
    from: u64,
    /// The history, ending in the capture.
    history: Arc<History>,
    /// The correspondence from the snapshot it was compared with to the capture; empty when
    /// the two are the same snapshot.
    links: Vec<Link>,
    /// The text of the capture's files that were not cached yet.
    texts: HashMap<ObjectId, Arc<FullText>>,
    /// Why the watch could not be walked again, if it could not.
    trouble: Option<String>,
}

impl Model {
    /// Starts following the working tree, for a review of it with `[review] live` on.
    pub(super) fn start_live(&mut self) {
        let Some(review) = self.review.clone() else {
            return;
        };
        if !self.settings.live || !matches!(review.target(), ReviewTarget::Worktree { .. }) {
            return;
        }
        self.live = Some(Live::default());
        let repo = review.repo().clone();
        self.spawn(Subject::Always(Because::Live), move || {
            Outcome::Watching(
                Watch::start(repo)
                    .map(|(watch, waiter)| Box::new(Started { watch, waiter }))
                    .map_err(|err| causes(&err)),
            )
        });
    }

    /// Whether the review follows the working tree.
    #[must_use]
    pub fn following(&self) -> bool {
        self.live.is_some()
    }

    /// Takes in the watch that started, or why the working tree cannot be watched, and takes
    /// the first capture, which sees every change made before the watch started.
    pub(super) fn watching(&mut self, started: Result<Box<Started>, String>) {
        let Some(live) = &mut self.live else {
            return;
        };
        let every = match started {
            Ok(started) => {
                let Started { watch, waiter } = *started;
                live.watch = Some(Arc::new(watch));
                live.walked = Some(Instant::now());
                self.wait_for_changes(waiter);
                RECONCILE
            }
            Err(err) => {
                self.effect(Effect::Log(format!(
                    "cannot watch the working tree, so it is captured every {} s: {err}",
                    POLL.as_secs()
                )));
                POLL
            }
        };
        self.capture(false);
        self.after(
            every,
            Subject::Always(Because::Live),
            Outcome::Tick(Tick::Reconcile),
        );
    }

    /// Waits on a thread of its own for the working tree to change.
    fn wait_for_changes(&mut self, mut waiter: Waiter) {
        self.effect(Effect::Wait(Box::new(move || {
            let seen = waiter.wait();
            Completion {
                subject: Subject::Always(Because::Live),
                outcome: Box::new(Outcome::Waited(Box::new(WaitedFor {
                    waiter,
                    waited: seen,
                }))),
            }
        })));
    }

    /// Takes in a change of the working tree: captures it, and waits for the next.
    pub(super) fn waited(&mut self, waited: WaitedFor) {
        let WaitedFor { mut waiter, waited } = waited;
        let Some(live) = &mut self.live else {
            return;
        };
        if waited == Waited::Closed {
            return;
        }
        if let Some(trouble) = waiter.take_problem()
            && !std::mem::replace(&mut live.troubled, true)
        {
            self.effect(Effect::Log(format!(
                "the working tree's watch may miss changes, which the capture every {} s \
                 catches: {trouble}",
                RECONCILE.as_secs()
            )));
        }
        self.wait_for_changes(waiter);
        self.capture(false);
    }

    /// The periodic capture, which walks the watched tree again first every [`REWALK`].
    pub(super) fn reconcile(&mut self) {
        let Some(live) = &mut self.live else {
            return;
        };
        let every = if live.watch.is_some() {
            RECONCILE
        } else {
            POLL
        };
        let rewalk = live.walked.is_none_or(|walked| walked.elapsed() >= REWALK);
        if rewalk {
            live.walked = Some(Instant::now());
        }
        self.capture(rewalk);
        self.after(
            every,
            Subject::Always(Because::Live),
            Outcome::Tick(Tick::Reconcile),
        );
    }

    /// Captures the working tree in the background and compares it with the snapshot on
    /// screen; with `rewalk`, walks the watched tree again first.
    ///
    /// One capture runs at a time, which keeps the pins of the captures in use safe from the
    /// next; a capture asked for meanwhile follows it.
    pub(super) fn capture(&mut self, rewalk: bool) {
        let (Some(review), Some(snapshot)) = (self.review.clone(), self.snapshot.clone()) else {
            return;
        };
        let known: HashSet<ObjectId> = self.blobs.texts.keys().cloned().collect();
        let from = self.shown;
        let Some(live) = &mut self.live else {
            return;
        };
        if live.capturing {
            live.again = Some(live.again.unwrap_or(false) || rewalk);
            return;
        }
        live.capturing = true;
        let watch = live.watch.clone().filter(|_| rewalk);
        let keep: Vec<ObjectId> = [
            Some(&snapshot.history),
            live.waiting.as_ref().map(|waiting| &waiting.history),
        ]
        .into_iter()
        .flatten()
        .filter_map(|history| provisional_head(history))
        .collect();
        self.spawn(Subject::Always(Because::Live), move || {
            Outcome::Captured(capture(&CaptureInput {
                review: &review,
                watch: watch.as_deref(),
                shown: &snapshot,
                from,
                keep: &keep,
                known: &known,
            }))
        });
    }

    /// Takes in a capture: shows it, or keeps it for when the reviewer is done, and starts the
    /// capture asked for meanwhile.
    pub(super) fn captured(&mut self, captured: Result<Box<Captured>, String>) {
        let Some(live) = &mut self.live else {
            return;
        };
        live.capturing = false;
        let again = live.again.take();
        match captured {
            Ok(mut captured) => {
                if let Some(trouble) = captured.trouble.take() {
                    self.effect(Effect::Log(format!(
                        "cannot walk the working tree again to watch it: {trouble}"
                    )));
                }
                self.offer(captured);
            }
            Err(err) => {
                self.effect(Effect::Log(format!(
                    "could not capture the working tree: {err}"
                )));
                self.sync_failed();
            }
        }
        if let Some(rewalk) = again {
            self.capture(rewalk);
        }
    }

    /// Shows `captured` when it was compared with the snapshot on screen and nothing holds it
    /// back, keeps it while something does, and takes another capture when it was compared with
    /// another snapshot or read before a mark was saved.
    fn offer(&mut self, captured: Box<Captured>) {
        if captured.from != self.shown || captured.history.version() < self.version {
            self.capture(false);
            return;
        }
        if self.live_waits() {
            if let Some(live) = &mut self.live {
                live.waiting = Some(captured);
            }
            return;
        }
        self.show_capture(*captured);
    }

    /// Shows the waiting capture once nothing holds it back; runs after every update.
    pub(super) fn show_waiting_capture(&mut self) {
        if self.live_waits() {
            return;
        }
        if let Some(waiting) = self.live.as_mut().and_then(|live| live.waiting.take()) {
            self.offer(waiting);
        }
    }

    /// Whether the reviewer is in the middle of something a new snapshot would pull the rows
    /// from under.
    fn live_waits(&self) -> bool {
        self.marking
            || !self.marks.is_empty()
            || self.overlay.is_some()
            || self.picker.is_some()
            || self.settings_panel.is_some()
            || self.dragging.is_some()
            || self.find.as_ref().is_some_and(|find| find.editing)
            || self.filter.as_ref().is_some_and(|filter| filter.editing)
            || !matches!(self.phase, Phase::Ready)
    }

    /// Shows the capture in place of the snapshot on screen, keeping the reviewer's place:
    /// the file, the hunk and the diff's top row, through the correspondence between the two;
    /// folds only of changes that stayed the same at a trusted position.
    fn show_capture(&mut self, captured: Captured) {
        let Captured {
            history,
            links,
            texts,
            ..
        } = captured;
        let (Some(old), Some(tip)) = (self.snapshot.clone(), history.current()) else {
            return;
        };
        if links.is_empty() && same_snapshot(&old.history, &history, old.revision, tip) {
            // The same snapshot read again, such as after a mark recorded it, or with marks
            // another window saved since: what shows is derived again.
            if history.version() > self.version {
                self.version = history.version();
                self.revisions = super::overview::summarize(&history, old.policy);
                if let Some(snapshot) = &mut self.snapshot {
                    snapshot.history = history;
                }
                self.refresh_tree();
                self.rebuild();
            }
            return;
        }
        let Ok(view) = history.view_under(tip, old.policy) else {
            return;
        };
        let moved = Moved::new(&links);
        let was = self.was_on();
        let old_target = self
            .review_target()
            .map(|(units, _)| units)
            .unwrap_or_default();
        let old_head = old
            .view()
            .and_then(|view| view.record().map(|record| record.head.clone()));

        self.snapshot = Some(Snapshot {
            history: Arc::clone(&history),
            revision: tip,
            policy: old.policy,
        });
        self.shown += 1;
        self.version = self.version.max(history.version());
        self.blobs.texts.extend(texts);
        self.blobs.retain_for(&view);
        let carry = |units: &HashSet<UnitId>| -> HashSet<UnitId> {
            units
                .iter()
                .filter_map(|&unit| moved.unchanged(unit.index()))
                .map(|index| view.id(index))
                .collect()
        };
        self.folds.units = carry(&self.folds.units);
        self.folds.opened = carry(&self.folds.opened);
        self.folds.expanded = carry(&self.folds.expanded);
        self.keep_place(&was, &view, &moved);
        self.protect_changes(&old, &view, &moved, &old_target);

        // Language servers run on the worktree of the head no longer shown.
        if view.record().map(|record| &record.head) != old_head.as_ref()
            && let Some(mut servers) = self.servers.try_lock()
        {
            servers.clear();
        }
        self.request_syntax();
        self.set_status(format!("The working tree changed. {}", self.summary()));
    }

    /// Where the reviewer is in the snapshot on screen.
    fn was_on(&self) -> WasOn {
        let offset = self.diff_scroll.target();
        let in_review_buffer = self.mode == Mode::Buffer
            && self
                .buffer
                .as_ref()
                .is_some_and(|buffer| matches!(buffer.origin, Origin::Review(_)));
        // In a review buffer, the hunk under its cursor is the one the keys act on.
        let hunk = match self.buffer_hunk() {
            Some(hunk) if in_review_buffer => hunk,
            _ => self.hunk,
        };
        WasOn {
            hunk: self.rows.hunks.get(hunk).cloned().unwrap_or_default(),
            hunk_line: self
                .hunk_rows
                .get(hunk)
                .and_then(|rows| self.rows.rows.get(rows.start))
                .and_then(|row| RowKey::of(row).head()),
            top: self.rows.rows.get(Anchor::at(offset).row).map(RowKey::of),
            anchor: diff_anchor(&self.rows, offset),
            buffer: self
                .buffer
                .as_ref()
                .filter(|_| in_review_buffer)
                .map(|buffer| buffer.cursor),
            tree: self.tree_node().map(|node| TreeSpot {
                kind: node.kind,
                path: node.path.clone(),
            }),
        }
    }

    /// Lays out the new snapshot where the reviewer `was`: the same file, or the one now where
    /// it was in the tree; the hunk its units went to, or the nearest; the same top row; the
    /// tree cursor on the same node; and a review buffer the reviewer was in, on the same hunk.
    ///
    /// Buffers of the old snapshot's files go, and so does the way back from a definition into
    /// one, since their change markers number the old snapshot's hunks.
    fn keep_place(&mut self, was: &WasOn, view: &RevisionView<'_>, moved: &Moved) {
        self.selection = None;
        let reviewed = |buffer: &super::buffer::Buffer| matches!(buffer.origin, Origin::Review(_));
        if self.buffer.as_ref().is_some_and(reviewed) {
            self.set_buffer(None);
        } else if let Some(buffer) = &mut self.buffer {
            buffer.previous = None;
        }
        self.rows = Arc::new(Layout::default());
        self.hunk_rows.clear();
        self.refresh_tree();
        if let Some(spot) = &was.tree {
            self.put_tree_cursor(spot.kind, &spot.path);
        }

        if !self
            .selected
            .as_deref()
            .is_some_and(|path| state::is_file(&self.tree, path))
        {
            self.selected = self.file_near_cursor();
        }
        if let Some(path) = self.selected.clone() {
            self.places.remove(&path);
        }
        // Places kept for other files name units of the old snapshot; each follows its unit to
        // the new one, or forgets it when the unit is gone.
        let follow =
            |unit: review_core::UnitId| moved.any(unit.index()).map(|index| view.id(index));
        for place in self.places.values_mut() {
            place.unit = place.unit.and_then(follow);
            place.key = place
                .key
                .and_then(|key| follow(key.unit()).map(|unit| key.with_unit(unit)));
        }

        self.hunk = 0;
        self.build_selected();
        let followed = was
            .hunk
            .iter()
            .filter_map(|unit| moved.any(unit.index()))
            .map(|index| view.id(index))
            .find_map(|unit| self.rows.hunks.iter().position(|hunk| hunk.contains(&unit)));
        self.hunk = followed.unwrap_or_else(|| self.nearest_hunk(was.hunk_line));

        let top = was
            .top
            .and_then(|key| {
                let unit = moved.any(key.unit().index()).map(|index| view.id(index));
                key.find_moved(unit, &self.rows)
            })
            .map_or(was.anchor, |row| Anchor { row, ..was.anchor });
        let viewport = self.grid().viewports.diff;
        self.diff_scroll.stop();
        self.diff_scroll.jump(frame::clamp(
            diff_offset(&self.rows, top),
            self.rows.rows.len(),
            viewport,
        ));

        if let Some(cursor) = was.buffer {
            // The buffer opens on the hunk the keys acted on; the old row stays only while it
            // is still on that hunk.
            self.enter_buffer();
            if let Some(start) = self.buffer.as_ref().map(|buffer| buffer.cursor) {
                self.set_buffer_cursor(cursor.row, cursor.col);
                if !was.hunk.is_empty() && self.buffer_hunk() != Some(self.hunk) {
                    self.set_buffer_cursor(start.row, start.col);
                }
            }
        }
        if self.selected.is_none() {
            self.enter_mode(Mode::Tree);
        } else if self.mode == Mode::Buffer && self.buffer.is_none() {
            self.enter_mode(Mode::Diff);
        }
        // Outside the tree, its cursor stays on the file that shows.
        if self.mode != Mode::Tree
            && let Some(path) = self.selected.clone()
        {
            self.put_tree_cursor(review_core::tree::NodeKind::File, &path);
        }
    }

    /// Puts the tree cursor on the visible node of `kind` at `path`, when there is one.
    fn put_tree_cursor(&mut self, kind: review_core::tree::NodeKind, path: &str) {
        let visible = self.visible_tree();
        if let Some(position) = visible.iter().position(|&index| {
            self.tree
                .get(index)
                .is_some_and(|node| node.kind == kind && node.path == path)
        }) {
            self.tree_cursor = position;
        }
    }

    /// Records which units changed: those without an unchanged link from the snapshot `old`
    /// changed now, and the others when their source last changed.
    ///
    /// What the review keys act on now changed now too unless it is exactly where what they
    /// acted on before, `old_target`, went: a hunk, file or folder the reviewer was moved onto
    /// is new under the keys, however unchanged it is.
    fn protect_changes(
        &mut self,
        old: &Snapshot,
        view: &RevisionView<'_>,
        moved: &Moved,
        old_target: &[UnitId],
    ) {
        let now = Instant::now();
        let went: HashSet<UnitId> = old_target
            .iter()
            .filter_map(|unit| moved.any(unit.index()))
            .map(|index| view.id(index))
            .collect();
        let target = self
            .review_target()
            .map(|(units, _)| units)
            .unwrap_or_default();
        let target = if target.iter().all(|unit| went.contains(unit)) {
            Vec::new()
        } else {
            target
        };
        let Some(live) = &mut self.live else {
            return;
        };
        let previous: HashMap<UnitIndex, Instant> = std::mem::take(&mut live.changed)
            .into_iter()
            .filter(|(unit, _)| unit.revision() == old.revision)
            .map(|(unit, at)| (unit.index(), at))
            .collect();
        for unit in view.unit_ids() {
            let at = match moved.unchanged_from(unit.index()) {
                Some(older) => previous.get(&older).copied(),
                None => Some(now),
            };
            if let Some(at) = at {
                live.changed.insert(unit, at);
            }
        }
        for unit in target {
            live.changed.insert(unit, now);
        }
    }

    /// The file at the tree cursor, or the nearest file before it, or the first.
    fn file_near_cursor(&self) -> Option<String> {
        let visible = self.visible_tree();
        let cursor = self.tree_cursor.min(visible.len().saturating_sub(1));
        let file = |index: &usize| {
            self.tree
                .get(*index)
                .filter(|node| node.kind == review_core::tree::NodeKind::File)
                .map(|node| node.path.clone())
        };
        visible
            .get(..=cursor)
            .and_then(|before| before.iter().rev().find_map(file))
            .or_else(|| visible.iter().find_map(file))
    }

    /// The hunk whose first row is nearest the head line `line`, or the first.
    fn nearest_hunk(&self, line: Option<usize>) -> usize {
        let Some(line) = line else {
            return 0;
        };
        self.hunk_rows
            .iter()
            .enumerate()
            .filter_map(|(hunk, rows)| {
                let head = self
                    .rows
                    .rows
                    .get(rows.start)
                    .and_then(|row| RowKey::of(row).head())?;
                Some((hunk, head.abs_diff(line)))
            })
            .min_by_key(|&(_, distance)| distance)
            .map_or(0, |(hunk, _)| hunk)
    }

    /// Refuses a review key on `units` while one of them changed under it: within [`SETTLE`]
    /// of the change, or since before the press that is still repeating; says why and returns
    /// whether it refused.
    pub(super) fn refuses_changed(&mut self, units: &[UnitId], what: &str) -> bool {
        let Some(live) = &mut self.live else {
            return false;
        };
        let now = Instant::now();
        let press = match live.press {
            Some(press) if now.duration_since(press.last) < REPEAT => Press { last: now, ..press },
            _ => Press {
                started: now,
                last: now,
            },
        };
        live.press = Some(press);
        let changed = units
            .iter()
            .filter_map(|unit| live.changed.get(unit))
            .any(|&at| at >= press.started || now.duration_since(at) < SETTLE);
        if changed {
            self.set_status(format!(
                "Not marked: {what} just changed in the working tree. Read it, then press \
                 again."
            ));
        }
        changed
    }
}

/// Where the reviewer was in the snapshot on screen, to keep the place in the next one.
struct WasOn {
    /// The units of the selected hunk.
    hunk: Vec<UnitId>,
    /// The head line the selected hunk starts on.
    hunk_line: Option<usize>,
    /// The diff's top row.
    top: Option<RowKey>,
    /// Where the diff is scrolled.
    anchor: Anchor,
    /// The cursor of the buffer of a file under review, while the reviewer is in it.
    buffer: Option<crate::text::TextPos>,
    /// The tree node under the cursor.
    tree: Option<TreeSpot>,
}

/// A node of the file tree, by what it is and where.
struct TreeSpot {
    kind: review_core::tree::NodeKind,
    path: String,
}

/// What a capture job needs.
struct CaptureInput<'a> {
    review: &'a review_core::Review,
    watch: Option<&'a Watch>,
    shown: &'a Snapshot,
    from: u64,
    keep: &'a [ObjectId],
    known: &'a HashSet<ObjectId>,
}

/// Captures the working tree and compares it with the snapshot shown; runs in the background.
fn capture(input: &CaptureInput<'_>) -> Result<Box<Captured>, String> {
    let trouble = input
        .watch
        .and_then(|watch| watch.rewalk().err())
        .map(|err| causes(&err));
    let history = input
        .review
        .live_history(input.keep)
        .map_err(|err| causes(&err))?;
    let tip = history.current().ok_or("the review has no revision")?;
    let links = if same_snapshot(&input.shown.history, &history, input.shown.revision, tip) {
        Vec::new()
    } else {
        let shown = input
            .shown
            .view()
            .and_then(|view| view.analysis())
            .ok_or("the review on screen has no analysis")?;
        let captured = history
            .view(tip)
            .ok()
            .and_then(|view| view.analysis())
            .ok_or("the capture has no analysis")?;
        correspond(input.review.repo(), shown, captured)
            .map_err(|err| causes(&err))?
            .links
    };
    let texts = read_new_texts(input.review, &history, tip, input.known);
    Ok(Box::new(Captured {
        from: input.from,
        history: Arc::new(history),
        links,
        texts,
        trouble,
    }))
}

/// The text of revision `tip`'s head files that `known` does not hold, so the capture shows
/// whole, without its context lines arriving later.
fn read_new_texts(
    review: &review_core::Review,
    history: &History,
    tip: review_core::RevisionNumber,
    known: &HashSet<ObjectId>,
) -> HashMap<ObjectId, Arc<FullText>> {
    let mut texts = HashMap::new();
    let Some(analysis) = history.view(tip).ok().and_then(|view| view.analysis()) else {
        return texts;
    };
    let wanted: Vec<&ObjectId> = analysis
        .files
        .iter()
        .filter_map(|file| file.new.as_ref())
        .filter(|side| is_regular(side) && !known.contains(&side.oid))
        .map(|side| &side.oid)
        .collect();
    if wanted.is_empty() {
        return texts;
    }
    let Ok(mut reader) = review.repo().blob_reader() else {
        return texts;
    };
    for oid in wanted {
        if let Ok(review_core::git::Blob::Content(bytes)) = reader.read(oid)
            && !bytes.contains(&0)
        {
            texts.insert(
                oid.clone(),
                Arc::new(FullText::new(&String::from_utf8_lossy(&bytes))),
            );
        }
    }
    texts
}

/// Whether revision `old` of `before` and revision `new` of `after` are the same snapshot:
/// the same number, base and head.
pub(super) fn same_snapshot(
    before: &History,
    after: &History,
    old: review_core::RevisionNumber,
    new: review_core::RevisionNumber,
) -> bool {
    let record = |history: &History, number| {
        history.view(number).ok().and_then(|view| {
            view.record()
                .map(|record| (record.base.clone(), record.head.clone()))
        })
    };
    old == new && record(before, old).is_some() && record(before, old) == record(after, new)
}

/// The head of `history`'s provisional revision, if it ends in one.
fn provisional_head(history: &History) -> Option<ObjectId> {
    let tip = history.current()?;
    if !history.is_provisional(tip) {
        return None;
    }
    history
        .view(tip)
        .ok()?
        .record()
        .map(|record| record.head.clone())
}

/// The correspondence from the old snapshot to the new one, by unit index.
struct Moved {
    /// Where each old unit went, by any link that is not ambiguous.
    any: HashMap<UnitIndex, UnitIndex>,
    /// Where each old unit went unchanged, at a trusted position.
    unchanged: HashMap<UnitIndex, UnitIndex>,
    /// Where each new unit came from unchanged.
    unchanged_from: HashMap<UnitIndex, UnitIndex>,
}

impl Moved {
    fn new(links: &[Link]) -> Self {
        let mut moved = Self {
            any: HashMap::new(),
            unchanged: HashMap::new(),
            unchanged_from: HashMap::new(),
        };
        for link in links {
            if link.kind != LinkKind::Ambiguous {
                moved.any.entry(link.older).or_insert(link.newer);
            }
            if link.kind == LinkKind::Unchanged {
                moved.unchanged.insert(link.older, link.newer);
                moved.unchanged_from.insert(link.newer, link.older);
            }
        }
        moved
    }

    fn any(&self, older: UnitIndex) -> Option<UnitIndex> {
        self.any.get(&older).copied()
    }

    fn unchanged(&self, older: UnitIndex) -> Option<UnitIndex> {
        self.unchanged.get(&older).copied()
    }

    fn unchanged_from(&self, newer: UnitIndex) -> Option<UnitIndex> {
        self.unchanged_from.get(&newer).copied()
    }
}

/// `err` and every cause under it, joined by `: `.
fn causes(err: &dyn std::error::Error) -> String {
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
impl Live {
    /// Moves every change `by` into the past, as if that much time passed since.
    pub(super) fn age(&mut self, by: Duration) {
        for at in self.changed.values_mut() {
            *at = at.checked_sub(by).unwrap_or(*at);
        }
        self.press = None;
    }

    /// Holds a review key down since `ago`, repeating until now.
    pub(super) fn hold_since(&mut self, ago: Duration) {
        let now = Instant::now();
        self.press = Some(Press {
            started: now.checked_sub(ago).unwrap_or(now),
            last: now,
        });
    }

    /// Whether a capture waits for the reviewer.
    pub(super) fn has_waiting(&self) -> bool {
        self.waiting.is_some()
    }
}
