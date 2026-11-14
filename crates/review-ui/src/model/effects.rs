//! What the model asks of the front end that holds it, the one way a front end changes it, and
//! the one way the outcome of background work lands.
//!
//! The model never runs anything itself: background work, timers, the clipboard, the window
//! title and quitting are [`Effect`]s it queues while it changes.
//! A front end changes the model only through [`Driver::update`], which runs the model's
//! after-update step and hands back the effects queued meanwhile, so no change can skip it.
//!
//! A job or a timer hands back a [`Completion`]: what it produced, and the subject it was
//! computed for, which every job and timer must name when it starts.
//! [`Model::complete`] checks the subject against what shows, in one place, before the outcome
//! lands, so no handler ever sees an outcome computed for something that no longer shows.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use review_core::{ObjectId, RevisionNumber};

use super::buffer::{Definition, Lookup};
use super::marks::{Marked, Refreshed};
use super::picker::{Found, PreviewText, Query, Unchanged};
use super::sync::Tick;
use super::{Model, Opened};
use crate::rows::FullText;
use crate::state::{SideKey, SyntaxKey};
use crate::syntax::{Resolved, SideSyntax};

/// What a background job or a timer hands back to the UI thread: what it produced, and what it
/// was computed for.
///
/// A front end hands it to [`Model::complete`], which lands it only while its subject still
/// shows.
pub struct Completion {
    pub(super) subject: Subject,
    /// On the heap, so a timer's effect stays as small as the others.
    pub(super) outcome: Box<Outcome>,
}

/// Work the model wants done off the UI thread, such as git, analysis or a language server.
pub type Job = Box<dyn FnOnce() -> Completion + Send>;

/// What a job or a timer was computed for, which must still show when it ends for its outcome
/// to land.
///
/// Every job and timer names its subject when it starts.
/// One whose outcome lands whatever changed meanwhile says so, and why, as [`Self::Always`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Subject {
    /// Lands whatever changed while the job ran, for the reason given.
    Always(Because),
    /// The tree cursor's rest of the preview with this number, while it is the newest.
    Preview(u64),
    /// The open picker's search with this generation, while it is the picker's newest.
    Search(u64),
    /// The open picker with this identity.
    Picker(u64),
    /// A definition lookup from this buffer of this revision, while the review is trusted.
    Lookup(Lookup),
    /// This revision, while it shows.
    Revision(RevisionNumber),
}

/// Why an outcome lands whatever changed while its job ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Because {
    /// It opens the review, before which nothing showed.
    Opening,
    /// It reports a save, of a mark or of the folded folders, which is always recorded: a
    /// failure must reach the reviewer, and quitting waits for it.
    Save,
    /// It brings the review's history up to date, which only adds what is newer.
    Refresh,
    /// It fills caches keyed by blob, whose content an object ID never lets go stale.
    Content,
    /// It only marks the passing of time, for what shows how long ago something happened or
    /// that something runs.
    Clock,
}

/// What a job or a timer produced.
pub(super) enum Outcome {
    /// The review opened, or why it could not.
    Opened(Result<Box<Opened>, String>),
    /// File texts read in the background.
    TextsRead(TextsRead),
    /// One file's highlights, made in the background.
    Highlighted(Highlighted),
    /// A mark saved or refused.
    Marked(Marked),
    /// A fetch of the latest changes.
    Refreshed(Refreshed),
    /// The folded folders saved, or why not.
    FoldersSaved(Result<(), review_core::Error>),
    /// Where a definition is, or why the lookup failed.
    Definition(Result<Definition, String>),
    /// What a picker's search found.
    Found(Found),
    /// The text of a file that the content search's preview needs.
    PreviewText(PreviewText),
    /// A file the review did not change, read to open as a buffer.
    Unchanged(Unchanged),
    /// The tree cursor rested on a previewed file.
    Rested,
    /// Typing paused in the content search, so its search can start.
    Paused(Query),
    /// A tick of the clock, for what shows the time.
    Tick(Tick),
}

/// File texts read in the background, by blob.
pub(super) struct TextsRead {
    /// The blobs asked for, which are no longer being read.
    pub(super) asked: Vec<ObjectId>,
    pub(super) texts: HashMap<ObjectId, Arc<FullText>>,
}

/// The versions of one file, highlighted in the background.
pub(super) struct Highlighted {
    /// The syntax each version resolved to, plain for binary and oversized ones.
    pub(super) resolved: HashMap<SideKey, Resolved>,
    /// The highlights made, by blob and syntax; a version that resolved to highlights already
    /// made, or to plain, has none here.
    pub(super) syntax: HashMap<SyntaxKey, Arc<SideSyntax>>,
}

/// Whether an outcome's subject still shows.
enum Freshness {
    /// It shows: the outcome lands.
    Current,
    /// It no longer shows: the outcome is dropped, and the note, when there is one, says so on
    /// the status line, where the reviewer waits for it.
    Stale(Option<&'static str>),
}

/// One thing the model asks of its front end.
pub enum Effect {
    /// Runs the job in the background and hands its completion to [`Model::complete`] on the
    /// UI thread.
    Spawn(Job),
    /// Hands the completion to [`Model::complete`] after the delay, as a timer: the tree
    /// cursor's rest before a previewed diff is built, the content search's pause for typing,
    /// or a tick of the clock for what shows the time.
    After(Duration, Completion),
    /// Puts the text on the clipboard.
    Copy(String),
    /// Puts the text in the primary selection, which the middle button pastes on Linux.
    CopyPrimary(String),
    /// Sets the window title.
    Title(String),
    /// Scrolls the cheat sheet or the overview, which the front end lays out and scrolls
    /// itself.
    ScrollSheet(SheetStep),
    /// Toggles full screen.
    Fullscreen,
    /// Quits the app; the model queues it once everything the reviewer did is saved.
    Quit,
    /// A diagnostic for the developer, such as why the review could not be opened.
    Log(String),
}

impl std::fmt::Debug for Effect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(_) => formatter.write_str("Spawn"),
            Self::After(delay, _) => formatter.debug_tuple("After").field(delay).finish(),
            Self::Copy(text) => formatter.debug_tuple("Copy").field(text).finish(),
            Self::CopyPrimary(text) => formatter.debug_tuple("CopyPrimary").field(text).finish(),
            Self::Title(title) => formatter.debug_tuple("Title").field(title).finish(),
            Self::ScrollSheet(step) => formatter.debug_tuple("ScrollSheet").field(step).finish(),
            Self::Fullscreen => formatter.write_str("Fullscreen"),
            Self::Quit => formatter.write_str("Quit"),
            Self::Log(message) => formatter.debug_tuple("Log").field(message).finish(),
        }
    }
}

/// How far the cheat sheet or the overview scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SheetStep {
    /// By lines of its text, down when positive.
    Lines(i32),
    /// By half its height, down when positive.
    HalfPages(i32),
}

/// What one update returned, and the effects it queued, which the front end must run.
#[must_use = "the effects must run, or background work and timers are lost"]
pub struct Updated<R> {
    /// What the change returned.
    pub value: R,
    /// The effects queued by the change and the after-update step, in order.
    pub effects: Vec<Effect>,
}

/// The model as a front end holds it: read freely, changed only through [`Self::update`].
pub struct Driver {
    model: Model,
}

impl Driver {
    /// Holds `model` for a front end.
    #[must_use]
    pub fn new(model: Model) -> Self {
        Self { model }
    }

    /// The model, to read and draw.
    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Changes the model with `change`, then runs its after-update step (which saves the
    /// folded folders when they changed), and returns what `change` returned with every effect
    /// queued since the last update.
    pub fn update<R>(&mut self, change: impl FnOnce(&mut Model) -> R) -> Updated<R> {
        let value = change(&mut self.model);
        let effects = self.model.after_update();
        Updated { value, effects }
    }
}

impl Model {
    /// Queues `job` to run in the background for `subject`; its outcome lands on the UI thread
    /// while `subject` still shows.
    pub(super) fn spawn(
        &mut self,
        subject: Subject,
        job: impl FnOnce() -> Outcome + Send + 'static,
    ) {
        self.effects
            .push(Effect::Spawn(Box::new(move || Completion {
                subject,
                outcome: Box::new(job()),
            })));
    }

    /// Queues `outcome` to land on the UI thread after `delay`, while `subject` still shows.
    pub(super) fn after(&mut self, delay: Duration, subject: Subject, outcome: Outcome) {
        self.effects.push(Effect::After(
            delay,
            Completion {
                subject,
                outcome: Box::new(outcome),
            },
        ));
    }

    /// Queues `effect`.
    pub(super) fn effect(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    /// Takes in what a job or a timer handed back: its end stops what marks it as running,
    /// and its outcome lands only while the subject it was computed for still shows.
    pub fn complete(&mut self, completion: Completion) {
        let Completion { subject, outcome } = completion;
        self.ended(&outcome);
        match self.freshness(subject) {
            Freshness::Current => self.land(*outcome),
            Freshness::Stale(note) => {
                if let Some(note) = note {
                    self.set_status(note);
                }
            }
        }
    }

    /// Stops what marks the job of `outcome` as running, whether or not its outcome lands.
    fn ended(&mut self, outcome: &Outcome) {
        match outcome {
            Outcome::TextsRead(read) => {
                self.highlighting = self.highlighting.saturating_sub(1);
                self.requested.finish(&read.asked);
            }
            Outcome::Highlighted(_) => {
                self.highlighting = self.highlighting.saturating_sub(1);
                self.highlight_running = false;
            }
            Outcome::Unchanged(_) => self.highlighting = self.highlighting.saturating_sub(1),
            Outcome::Marked(_) => self.marking = false,
            Outcome::Refreshed(_) => {
                self.refreshing = false;
                *self.sync.step.lock() = None;
            }
            Outcome::FoldersSaved(_) => self.folders.saving = false,
            Outcome::Definition(_) => {
                self.looking_up = false;
                // Trust was revoked while the lookup held the servers, so they stop now.
                if !self.trusted {
                    self.servers.lock().clear();
                }
            }
            // The opening ends when the review shows; a picker searches until its newest search
            // lands, and reads a preview until that picker takes the text in.
            Outcome::Opened(_)
            | Outcome::Found(_)
            | Outcome::PreviewText(_)
            | Outcome::Paused(_)
            | Outcome::Tick(_)
            | Outcome::Rested => {}
        }
    }

    /// Whether `subject` still shows.
    fn freshness(&self, subject: Subject) -> Freshness {
        let current = match subject {
            Subject::Always(_) => true,
            Subject::Preview(preview) => self.preview == Some(preview),
            Subject::Search(generation) => self.picker_generation() == Some(generation),
            Subject::Picker(id) => self.picker.as_ref().is_some_and(|picker| picker.id == id),
            Subject::Lookup(lookup) => {
                // Trust was revoked during the lookup: its answer says nothing.
                if !self.trusted {
                    return Freshness::Stale(None);
                }
                let shown = self
                    .buffer
                    .as_ref()
                    .map(|buffer| buffer.id)
                    .zip(self.snapshot.as_ref().map(|snapshot| snapshot.revision));
                if shown != Some((lookup.buffer, lookup.revision)) {
                    return Freshness::Stale(Some(
                        "The definition lookup finished for a file no longer shown.",
                    ));
                }
                true
            }
            Subject::Revision(revision) => {
                self.snapshot.as_ref().map(|snapshot| snapshot.revision) == Some(revision)
            }
        };
        if current {
            Freshness::Current
        } else {
            Freshness::Stale(None)
        }
    }

    /// Lands `outcome`, whose subject still shows.
    fn land(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Opened(opened) => self.opened(opened),
            Outcome::TextsRead(read) => self.texts_read(read),
            Outcome::Highlighted(highlighted) => self.highlighted(highlighted),
            Outcome::Marked(marked) => self.marked(marked),
            Outcome::Refreshed(refreshed) => self.refreshed(refreshed),
            Outcome::FoldersSaved(saved) => self.folders_saved(saved),
            Outcome::Definition(found) => self.definition_found(found),
            Outcome::Found(found) => self.picker_found(found),
            Outcome::PreviewText(text) => self.preview_read(text),
            Outcome::Unchanged(unchanged) => self.unchanged_read(unchanged),
            Outcome::Rested => self.finish_preview(),
            Outcome::Paused(query) => self.search_picker(query),
            Outcome::Tick(tick) => self.ticked(tick),
        }
    }
}
