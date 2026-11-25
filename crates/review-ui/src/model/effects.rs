//! What the model asks of the front end that holds it, the one way a front end changes it, and
//! the one way the outcome of background work lands.
//!
//! The model never runs anything itself: background work, timers, the clipboard, the window
//! title and quitting are [`Effect`]s it queues while it changes.
//! A front end changes the model only through [`Driver::update`], and hands it what its jobs
//! and timers produced through [`Driver::complete`]; both run the model's after-update step and
//! hand back the effects queued meanwhile, so no change can skip it.
//! Once the app closes, only the second changes the model.
//!
//! A job or a timer hands back a [`Completion`]: what it produced, and the subject it was
//! computed for, which every job and timer must name when it starts.
//! [`Model::complete`] checks the subject against what shows, in one place, before the outcome
//! lands, so no handler ever sees an outcome computed for something that no longer shows.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use review_core::ObjectId;

use super::buffer::{Definition, Lookup, Unanswered};
use super::engine::Waiting;
use super::live::{Captured, Missed, WaitedFor};
use super::marks::{Marked, Refreshed};
use super::picker::{Found, PreviewText, Query, Unchanged};
use super::pulls::Looked;
use super::remember::ConfigSaved;
use super::sync::Tick;
use super::trust::{Decided, Trust};
use super::{Model, Opened};
use crate::rows::FullText;
use crate::state::{SideKey, SyntaxKey};
use crate::syntax::{Resolved, SideSyntax};

/// What a background job or a timer hands back to the UI thread: what it produced, and what it
/// was computed for.
///
/// A front end hands it to [`Driver::complete`], which lands it only while its subject still
/// shows.
pub struct Completion {
    pub(super) subject: Subject,
    /// On the heap, so a timer's effect stays as small as the others.
    pub(super) outcome: Box<Outcome>,
}

/// Work the model wants done off the UI thread, such as git, analysis or a language server,
/// which a front end can only start on a thread of its own, never run in place:
///
/// ```compile_fail
/// fn run_in_place(job: review_ui::model::Job) -> review_ui::model::Completion {
///     job()
/// }
/// ```
pub struct Job {
    work: Box<dyn FnOnce() -> Completion + Send>,
    /// Whether it waits for something outside the app, which the model's tests run apart.
    #[cfg(test)]
    pub(super) waits: bool,
}

impl Job {
    /// `work`, as a job.
    pub(super) fn new(work: impl FnOnce() -> Completion + Send + 'static) -> Self {
        Self {
            work: Box::new(work),
            #[cfg(test)]
            waits: false,
        }
    }

    /// `work`, as a job that waits for something outside the app, such as the working tree
    /// changing, for as long as the app runs.
    pub(super) fn waiting(work: impl FnOnce() -> Completion + Send + 'static) -> Self {
        Self {
            work: Box::new(work),
            #[cfg(test)]
            waits: true,
        }
    }

    /// Runs the job on a thread of its own and hands its completion to `deliver` there, which
    /// passes it on to [`Driver::complete`] on the UI thread.
    pub fn start(self, deliver: impl FnOnce(Completion) + Send + 'static) {
        std::thread::spawn(move || deliver((self.work)()));
    }

    /// Runs the job in place, as the model's tests do in the order they pick.
    #[cfg(test)]
    pub(super) fn run(self) -> Completion {
        (self.work)()
    }
}

/// What a job or a timer was computed for, which must still show when it ends for its outcome
/// to land.
///
/// Every job and timer names its subject when it starts.
/// One whose outcome lands whatever changed meanwhile says so, and why, as [`Self::Always`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Subject {
    /// Lands whatever changed while the job ran, for the reason given.
    Always(Because),
    /// The open picker's search with this generation, while it is the picker's newest.
    Search(u64),
    /// The open picker with this identity.
    Picker(u64),
    /// A definition lookup from this buffer of this snapshot, while the trust epoch it started in
    /// lasts.
    Lookup(Lookup),
    /// The snapshot with this generation, while it shows; see [`Model::shown`].
    Shown(u64),
    /// A reading of trust in the trust epoch this was, while it lasts; see the `trust` module.
    Trust(u64),
}

/// Why an outcome lands whatever changed while its job ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Because {
    /// It opens the review, before which nothing showed.
    Opening,
    /// It reports a save, of a mark, a decision on trust, the folded folders or a setting,
    /// which is always recorded: a failure must reach the reviewer, and quitting waits for it.
    Save,
    /// It brings the review's history up to date, which only adds what is newer.
    Refresh,
    /// It fills caches keyed by blob, whose content an object ID never lets go stale.
    Content,
    /// It only marks the passing of time, for what shows how long ago something happened or
    /// that something runs.
    Clock,
    /// It follows the working tree: a watch that started, a change, a capture, or the time for
    /// the next capture; a capture checks for itself which snapshot it was compared with, and
    /// a lookup of the pull request which branch it asked about.
    Live,
    /// It stops the engine as the app quits, which quitting waits for.
    Stop,
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
    /// A decision on trust recorded, or why it could not be.
    Decided(Decided),
    /// Whether tools may run on the snapshot on screen, read in the background.
    TrustRead(Trust),
    /// A fetch of the latest changes.
    Refreshed(Refreshed),
    /// The folded folders saved, or why not.
    FoldersSaved(Result<(), review_core::Error>),
    /// An edit of the configuration file saved, or why not.
    ConfigSaved(ConfigSaved),
    /// Where a definition is, or why the lookup found none.
    Definition(Result<Definition, Unanswered>),
    /// What a picker's search found.
    Found(Found),
    /// The text of a file that the content search's preview needs.
    PreviewText(PreviewText),
    /// A file the review did not change, read to open as a buffer.
    Unchanged(Unchanged),
    /// Typing paused in the content search, so its search can start.
    Paused(Query),
    /// A tick of the clock, for what shows the time.
    Tick(Tick),
    /// The watch on the working tree started, with what waits on it, or why it could not.
    Watching(Result<Box<Waiting>, String>),
    /// The working tree changed, or the watch ended.
    Waited(Box<WaitedFor>),
    /// A capture of the working tree, or why it shows nothing new.
    Captured(Result<Box<Captured>, Missed>),
    /// A lookup of the checked-out branch's pull request.
    PullRequest(Box<Looked>),
    /// The engine stopped, as the app quits.
    Disposed,
}

/// File texts read in the background, by blob.
pub(super) struct TextsRead {
    /// The blobs asked for, which are no longer being read.
    pub(super) asked: Vec<ObjectId>,
    pub(super) texts: HashMap<ObjectId, Arc<FullText>>,
}

/// The versions of one file, highlighted in the background.
pub(super) struct Highlighted {
    /// The job that made it, which its landing retires.
    pub(super) job: super::highlights::JobId,
    /// Whether the job was cancelled, which leaves what it made possibly partial.
    pub(super) cancelled: bool,
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
    /// Runs the job in the background and hands its completion to [`Driver::complete`] on the
    /// UI thread; see [`Job::start`].
    Spawn(Job),
    /// Hands the completion to [`Driver::complete`] after the delay, as a timer: the tree
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
    /// What the change returned, or `None` once the app closes, when it did not run.
    pub value: Option<R>,
    /// The effects queued by the change and the after-update step, in order.
    pub effects: Vec<Effect>,
}

/// The model as a front end holds it: read freely, changed by the reviewer only through
/// [`Self::update`], which takes nothing once the app closes, and by its background work and
/// timers through [`Self::complete`].
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
    ///
    /// Once everything is saved for quitting and the engine stops, `change` does not run and
    /// nothing is returned: no key, click or anything else the reviewer does acts any more,
    /// since nothing it changes would be saved.
    pub fn update<R>(&mut self, change: impl FnOnce(&mut Model) -> R) -> Updated<R> {
        if self.model.quitting.over() {
            return Updated {
                value: None,
                effects: Vec::new(),
            };
        }
        let value = change(&mut self.model);
        let effects = self.model.after_update();
        Updated {
            value: Some(value),
            effects,
        }
    }

    /// Hands the model what a job or a timer handed back, which lands only while what it was
    /// computed for still shows, even once the app closes, since quitting waits for the engine's
    /// job to stop it; returns every effect queued since the last update.
    pub fn complete(&mut self, completion: Completion) -> Vec<Effect> {
        self.model.complete(completion);
        self.model.after_update()
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
        // From the moment the job is queued, so the trace shows its wait for a thread too.
        let span = tracing::info_span!("job", ?subject);
        self.effects
            .push(Effect::Spawn(Job::new(move || Completion {
                subject,
                outcome: Box::new(span.in_scope(job)),
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
    #[tracing::instrument(skip_all, fields(subject = ?completion.subject))]
    pub(super) fn complete(&mut self, completion: Completion) {
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
            Outcome::Unchanged(_) => self.highlighting = self.highlighting.saturating_sub(1),
            Outcome::Marked(_) | Outcome::Decided(_) => self.saving = false,
            Outcome::Refreshed(_) => {
                self.refreshing = false;
                self.sync.step.set(None);
            }
            Outcome::FoldersSaved(_) => self.folders.saving = false,
            Outcome::ConfigSaved(_) => self.config_saving = false,
            Outcome::Definition(_) => self.looking_up = false,
            // The opening ends when the review shows; a picker searches until its newest search
            // lands, and reads a preview until that picker takes the text in; every highlight job
            // lands, and its landing retires it; a reading of trust that no longer counts leaves
            // the trust to whatever moved the epoch on.
            Outcome::Opened(_)
            | Outcome::Highlighted(_)
            | Outcome::TrustRead(_)
            | Outcome::Found(_)
            | Outcome::PreviewText(_)
            | Outcome::Paused(_)
            | Outcome::Tick(_)
            | Outcome::Watching(_)
            | Outcome::Waited(_)
            | Outcome::Captured(_)
            | Outcome::PullRequest(_)
            | Outcome::Disposed => {}
        }
    }

    /// Whether `subject` still shows.
    fn freshness(&self, subject: Subject) -> Freshness {
        let current = match subject {
            Subject::Always(_) => true,
            Subject::Search(generation) => self.picker_generation() == Some(generation),
            Subject::Picker(id) => self.picker.as_ref().is_some_and(|picker| picker.id == id),
            Subject::Lookup(lookup) => {
                let shown = self.buffer.as_ref().map(|buffer| (buffer.id, self.shown));
                if shown != Some((lookup.buffer, lookup.shown)) {
                    return Freshness::Stale(Some(
                        "The definition lookup finished for a file no longer shown.",
                    ));
                }
                // Trust was decided or denied during the lookup, even if granted again since:
                // its answer says nothing.
                self.epoch() == lookup.epoch
            }
            Subject::Shown(generation) => self.snapshot.is_some() && self.shown == generation,
            Subject::Trust(epoch) => self.epoch() == epoch,
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
            Outcome::Opened(opened) => self.offer_opened(opened),
            Outcome::TextsRead(read) => self.texts_read(read),
            Outcome::Highlighted(highlighted) => self.highlighted(highlighted),
            Outcome::Marked(marked) => self.marked(marked),
            Outcome::Decided(decided) => self.decided(decided),
            Outcome::TrustRead(trust) => self.take_trust(trust),
            Outcome::Refreshed(refreshed) => self.refreshed(refreshed),
            Outcome::FoldersSaved(saved) => self.folders_saved(saved),
            Outcome::ConfigSaved(saved) => self.config_saved(saved),
            Outcome::Definition(found) => self.definition_found(found),
            Outcome::Found(found) => self.picker_found(found),
            Outcome::PreviewText(text) => self.preview_read(text),
            Outcome::Unchanged(unchanged) => self.unchanged_read(unchanged),
            Outcome::Paused(query) => self.search_picker(query),
            Outcome::Tick(tick) => self.ticked(tick),
            Outcome::Watching(started) => self.watching(started),
            Outcome::Waited(waited) => self.waited(*waited),
            Outcome::Captured(captured) => self.captured(captured),
            Outcome::PullRequest(looked) => self.looked_up(*looked),
            Outcome::Disposed => self.closed(),
        }
    }
}
