//! The review window's model, in lazygit's shape: the changed files on the left, the selected
//! file's diff on the right, and the whole file as a read-only buffer on request.
//!
//! The model knows no UI toolkit.
//! A front end draws it, turns its input into [`Model::dispatch`] and typed calls through
//! [`Driver::update`], hands each frame's [`Geometry`] to the layout phase ([`Model::layout`]),
//! and runs the [`Effect`]s the model queues: background jobs, timers, the clipboard, the
//! window title.
//!
//! The model keeps three kinds of state apart:
//!
//! - the [`state::Snapshot`] on screen, one revision of one history, and the file text and
//!   highlights it shows, cached by blob identity;
//! - the workspace: the selected file, the mode that owns the keyboard, the selected hunk, folds
//!   and scroll positions;
//! - the marks waiting for the engine, each with the exact units and the revision they were
//!   shown in.
//!
//! Everything slow (git, analysis, the event log, highlighting, language servers) runs as a
//! background [`Job`].
//! Its [`Completion`] names what it was computed for, and [`Model::complete`] checks that
//! against what shows before it lands, so no outcome can land on the wrong view.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use review_core::tree::{NodeKind, TreeNode};
use review_core::{History, MarkVerdict, ObjectId, Review, RevisionNumber, UnitId};

use crate::frame::{Geometry, ListScroll, Scale, TextPan};
use crate::front_end::FrontEnd;
use crate::keymap::Area;
use crate::lsp::LanguageServer;
use crate::rows::{self, Folds, Layout};
use crate::state::{self, MarkJob, Snapshot};
use crate::syntax::SideSyntax;
use crate::text::{Pane, Selection};
use crate::{find, keymap};

mod actions;
pub mod buffer;
mod diff;
mod effects;
mod fields;
mod folders;
mod layout;
mod marks;
pub mod overview;
pub mod picker;
pub mod present;
mod quit;
mod search;
mod select;
pub mod settings;
pub mod sync;
#[cfg(test)]
mod tests;
mod tree;

pub use actions::Action;
use effects::{Because, Highlighted, Outcome, Subject};
pub use effects::{Completion, Driver, Effect, Job, SheetStep, Updated};
pub use fields::{Field, FieldState};
pub use quit::{QuitKind, Quitting};
pub use select::{TextPress, list_of};
pub use sync::{Sync, SyncStep};

/// How many unchanged lines each expansion of a gap reveals.
const GAP_STEP: usize = 20;

/// How long go-to-definition waits for a language server that is still indexing.
const DEFINITION_DEADLINE: Duration = Duration::from_secs(120);

/// The smallest and largest code size zoom and the configuration allow, in the front end's
/// units: logical pixels in a window.
pub const ZOOM_RANGE: Range<f32> = 9.0..40.0;

/// How long the tree cursor rests before a previewed file's diff is built; a held key repeats
/// faster than this.
const PREVIEW_REST: Duration = Duration::from_millis(120);

/// View settings from the configuration file, and the look as the reviewer changes it.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Where the configuration file is, for remembering choices made in the app.
    pub config_path: Option<PathBuf>,
    /// The code size the window opens with, in logical pixels, within [`ZOOM_RANGE`].
    pub code_size: f32,
    /// Unchanged lines shown around each hunk.
    pub context: usize,
    /// Whether approved hunks fold, as the configuration sets it.
    pub approved: crate::config::Approved,
    /// Which units still need the reviewer, as the configuration sets it: whether proven
    /// formatting-only units count as approved without a mark.
    pub policy: review_core::ReviewPolicy,
    /// Whether a language server may start for go to definition, as `[lsp] enabled` sets it.
    pub language_servers: bool,
    /// The removed-line background, as `0xRRGGBB`, when the configuration overrides the
    /// theme's.
    pub removed: Option<u32>,
    /// The added-line background, likewise.
    pub added: Option<u32>,
    /// The look applied, and the one previewed over it while the settings list is open.
    pub appearance: crate::look::Appearance,
    /// Every theme the settings list offers.
    pub themes: Vec<crate::theme::Theme>,
    /// Every diff style, in the order `|` cycles them.
    pub styles: Vec<crate::look::DiffStyle>,
    /// The pane widths remembered from the last session.
    pub layout: PaneLayout,
    /// The keymap, after the configuration's overrides, without the actions the front end
    /// cannot honor.
    pub keymap: Vec<keymap::Binding>,
    /// The front end that draws the window, which decides what it can do and which of the
    /// configuration's settings are its own.
    pub front_end: FrontEnd,
    /// What the configuration got wrong, which the status line names once the review opens.
    pub problems: Vec<String>,
}

/// The files pane's width and whether it shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneLayout {
    /// The width, the front end's own, in its units: logical pixels in review-gui and cells
    /// in review-tui; see [`FrontEnd::sidebar_key`].
    pub sidebar: f32,
    /// Whether the files show, which every front end shares.
    pub sidebar_visible: bool,
}

/// An opened review with its history.
pub struct Loaded {
    /// The review.
    pub review: Review,
    /// Its revisions.
    pub history: History,
    /// How the review was found, such as the pull request of the current branch, which the
    /// status line says when the review opens.
    pub found: Option<String>,
}

/// Opens the review and loads its history, off the UI thread.
pub type Opener = Box<dyn FnOnce() -> Result<Loaded, String> + Send>;

/// Which part of the window owns the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The file tree; the diff previews the selected file.
    Tree,
    /// The selected file's diff, moving between hunks.
    Diff,
    /// The whole file as a read-only buffer.
    Buffer,
}

/// How the window divides its width, cycled with `+` and `_` as in lazygit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Files and diff side by side.
    Split,
    /// The diff or buffer across the whole width.
    Full,
}

/// A layer over the window that takes the keyboard while it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// The cheat sheet.
    Help,
    /// The pull request overview.
    Overview,
    /// The file finder or the content search; see [`Model::picker`].
    Picker,
    /// The settings list; see the `settings` module.
    Settings,
}

/// Whether the review is ready to show.
#[derive(Debug, Clone)]
pub enum Phase {
    /// The review is opening; the time it started.
    Loading(Instant),
    /// The review is open.
    Ready,
    /// The review could not be opened.
    Failed(String),
}

/// Where the reviewer was in a file, kept while other files show: the selected hunk, and the
/// unified row at the top, which survives any change of layout, density or font.
#[derive(Debug, Clone, Copy, Default)]
struct Place {
    hunk: usize,
    top: present::Anchor,
}

/// A `/` search in the diff or the buffer.
#[derive(Debug, Clone)]
pub struct Find {
    /// The mode it searches, [`Mode::Diff`] or [`Mode::Buffer`]; leaving that mode ends it.
    pub target: Mode,
    /// The search field's identity; see [`FieldState::id`].
    id: u64,
    /// What the field holds.
    pub query: String,
    /// Shared with each frame's rows rather than copied.
    pub matches: Arc<Vec<find::Match>>,
    /// The match the search is on, by index into [`Self::matches`].
    pub current: Option<usize>,
    /// Where the search started, so the matches of a query being typed are found from there,
    /// as vim's incremental search does.
    origin: find::Spot,
    /// In the diff, the hunk the current match selected; once another hunk is selected, `n`
    /// and `N` search from there instead.
    hunk: Option<usize>,
    /// Whether the bar takes typing; after Enter, the matches stay for `n` and `N`.
    pub editing: bool,
}

/// The `/` filter of the file tree, which narrows the tree to the files whose path contains its
/// query and stays until cleared.
#[derive(Debug, Clone)]
pub struct Filter {
    /// The filter field's identity; see [`FieldState::id`].
    id: u64,
    /// What the field holds.
    pub query: String,
    /// Whether the bar takes typing; after Enter, the filter stays and the tree takes the keys.
    pub editing: bool,
}

/// Running language servers by program and worktree.
type Servers = Arc<Mutex<HashMap<(&'static str, PathBuf), LanguageServer>>>;

/// File text and highlights by blob, filled in the background; content never changes under an
/// object ID, so these never go stale.
#[derive(Default)]
pub(crate) struct Blobs {
    texts: HashMap<ObjectId, Arc<rows::FullText>>,
    syntax: HashMap<ObjectId, Arc<SideSyntax>>,
}

impl Blobs {
    /// Keeps only the blobs `view` shows, so a long review's earlier revisions do not stay in
    /// memory for the session.
    fn retain_for(&mut self, view: &review_core::RevisionView<'_>) {
        let shown: HashSet<&ObjectId> = view
            .analysis()
            .into_iter()
            .flat_map(|analysis| &analysis.files)
            .flat_map(|file| [file.old.as_ref(), file.new.as_ref()])
            .flatten()
            .map(|side| &side.oid)
            .collect();
        self.texts.retain(|oid, _| shown.contains(oid));
        self.syntax.retain(|oid, _| shown.contains(oid));
    }
}

/// The review window's state and behavior; see the module documentation.
pub struct Model {
    /// Whether the review is open.
    pub phase: Phase,
    /// The open review.
    pub review: Option<Arc<Review>>,
    /// The revision on screen.
    pub snapshot: Option<Snapshot>,
    blobs: Blobs,
    /// Blobs being read or highlighted right now, so none is requested twice at once; an entry
    /// goes when its work completes, so a blob shown again after pruning is requested again.
    requested: state::Outstanding,
    /// What the review is about.
    pub info: Option<review_core::ReviewInfo>,
    /// Every revision, for the overview.
    pub revisions: Vec<overview::RevisionSummary>,
    /// The file tree with pending marks applied.
    pub tree: Vec<TreeNode>,
    /// The folded folders, saved with the review by the folders' saver.
    pub tree_collapsed: HashSet<String>,
    folders: folders::Saver,
    /// The visible tree nodes, from [`Self::update_visible_tree`].
    tree_visible: Arc<Vec<usize>>,
    /// The tree cursor, by position among the visible nodes.
    pub tree_cursor: usize,
    /// Where the tree is scrolled.
    pub(crate) tree_scroll: ListScroll,
    /// The file whose diff shows, by display path.
    pub selected: Option<String>,
    /// The mode that owns the keyboard when no overlay does.
    pub mode: Mode,
    /// How the window divides its width.
    pub screen: Screen,
    /// The layer over the window, when one shows.
    pub overlay: Option<Overlay>,
    /// The selected file's diff.
    pub rows: Arc<Layout>,
    /// A file previewed while the tree cursor moves fast: only its header shows until the
    /// cursor rests, then its diff is built. Holds the preview's number, so a later move
    /// supersedes an earlier one.
    pub preview: Option<u64>,
    previews: u64,
    /// When the tree cursor last moved, to tell a held key from a single press.
    last_move: Option<Instant>,
    /// The presentation [`Self::build_selected`] projects the rows for; only
    /// [`Self::transition`] changes it.
    presented: rows::Presentation,
    /// The code size in the front end's units, which every size in the window follows; only
    /// `Self::transition` changes it, so the lists keep their place.
    pub code_size: f32,
    /// The code size zoom resets to.
    default_code_size: f32,
    /// The side-by-side diff's code offset; see [`Self::diff_text`].
    pub text: TextPan,
    /// The unified diff's code offset, apart from the side-by-side one, since the two
    /// layouts' lines run to other widths.
    pub unified_text: TextPan,
    /// The buffer's code offset, which follows its cursor.
    pub buffer_text: TextPan,
    /// Whether the buffer's cursor waits to be revealed sideways until the buffer list has a
    /// width; see [`Self::reveal_buffer_column`].
    buffer_column_waits: bool,
    /// The settings list, while it is open.
    pub settings_panel: Option<settings::Panel>,
    /// The rows of each display hunk of [`Self::rows`].
    pub hunk_rows: Vec<Range<usize>>,
    /// The selected hunk, by index into the layout's hunks.
    pub hunk: usize,
    /// What folds, and the text and highlights the rows are laid out from.
    pub folds: Folds,
    places: HashMap<String, Place>,
    /// Where the diff is scrolled.
    pub diff_scroll: ListScroll,
    /// The file shown in full, in the buffer mode.
    pub buffer: Option<buffer::Buffer>,
    /// The last buffer identity handed out.
    buffer_ids: u64,
    /// Where the buffer is scrolled.
    pub(crate) buffer_scroll: ListScroll,
    /// The `/` search of the diff or the buffer.
    pub find: Option<Find>,
    /// The tree's `/` filter.
    pub filter: Option<Filter>,
    /// The open picker.
    pub picker: Option<picker::Picker>,
    /// Pickers opened and searches run, one count for both, so every answer names its search.
    picker_runs: u64,
    /// The last text field identity handed out.
    field_ids: u64,
    /// Marks waiting for the engine, saved one at a time.
    pub marks: VecDeque<MarkJob>,
    /// Whether a mark is being saved.
    pub marking: bool,
    /// Verdicts asked for but not yet recorded, shown at once.
    pub pending: HashMap<UnitId, MarkVerdict>,
    /// Whether a fetch of the latest changes runs.
    pub(crate) refreshing: bool,
    /// When the review last synced, and what a running fetch is doing.
    pub sync: Sync,
    /// Whether the app was asked to quit, and whether it may.
    pub quitting: Quitting,
    /// What the status line last said about quitting, so a note about a refused mark stays
    /// until what quitting waits for changes.
    quit_said: String,
    /// The pane widths and whether the files show.
    pub layout: PaneLayout,
    /// The status line.
    pub status: String,
    /// When the status line last reported a failure, for tinting it.
    pub failed_at: Option<Instant>,
    /// Whether a definition lookup runs.
    pub(crate) looking_up: bool,
    /// Background reads and highlighting jobs still running.
    pub(crate) highlighting: usize,
    /// Whether language servers may run on the reviewed code; see [`Review::trust`].
    pub trusted: bool,
    /// Whether trusting awaits a confirming second press.
    trust_pending: bool,
    /// The text selection in the diff or the buffer.
    pub selection: Option<Selection>,
    /// The pane a selection is being dragged in.
    pub dragging: Option<Pane>,
    /// The configuration and the look.
    pub settings: Settings,
    servers: Servers,
    /// How the front end's sizes follow the code size.
    scale: Scale,
    /// The frame the front end measured last.
    geometry: Geometry,
    /// Effects queued since the last update; see [`Driver::update`].
    effects: Vec<Effect>,
}

impl Model {
    /// A window for `settings` before its review opens, drawn in the sizes `scale` gives and
    /// opening in `geometry`; see [`Self::open`].
    #[must_use]
    pub fn new(settings: Settings, scale: Scale, geometry: Geometry) -> Self {
        let presented = present::opening(&settings, &scale, &geometry);
        Self {
            phase: Phase::Loading(Instant::now()),
            review: None,
            snapshot: None,
            blobs: Blobs::default(),
            requested: state::Outstanding::default(),
            info: None,
            revisions: Vec::new(),
            tree: Vec::new(),
            tree_collapsed: HashSet::new(),
            folders: folders::Saver::default(),
            tree_visible: Arc::new(Vec::new()),
            tree_cursor: 0,
            tree_scroll: ListScroll::default(),
            selected: None,
            mode: Mode::Tree,
            screen: Screen::Split,
            overlay: None,
            rows: Arc::new(Layout::default()),
            preview: None,
            previews: 0,
            last_move: None,
            presented,
            code_size: settings.code_size,
            default_code_size: settings.code_size,
            text: TextPan::default(),
            unified_text: TextPan::default(),
            buffer_text: TextPan::default(),
            buffer_column_waits: false,
            settings_panel: None,
            hunk_rows: Vec::new(),
            hunk: 0,
            folds: Folds {
                context: settings.context,
                approved: settings.approved,
                single_file: true,
                ..Folds::default()
            },
            places: HashMap::new(),
            diff_scroll: ListScroll::default(),
            buffer: None,
            buffer_ids: 0,
            buffer_scroll: ListScroll::default(),
            find: None,
            filter: None,
            picker: None,
            picker_runs: 0,
            field_ids: 0,
            marks: VecDeque::new(),
            marking: false,
            pending: HashMap::new(),
            refreshing: false,
            sync: Sync::default(),
            quitting: Quitting::No,
            quit_said: String::new(),
            layout: settings.layout,
            status: "Opening the review…".to_owned(),
            failed_at: None,
            looking_up: false,
            highlighting: 0,
            trusted: false,
            trust_pending: false,
            selection: None,
            dragging: None,
            settings,
            servers: Servers::default(),
            scale,
            geometry,
            effects: Vec::new(),
        }
    }

    /// Opens the review with `opener` in the background and shows it when it is ready.
    pub fn open(&mut self, opener: Opener) {
        self.spawn(Subject::Always(Because::Opening), move || {
            Outcome::Opened(open(opener).map(Box::new))
        });
        self.spin();
    }

    /// Shows the opened review, or why it could not be opened.
    fn opened(&mut self, opened: Result<Box<Opened>, String>) {
        match opened {
            Ok(opened) => {
                let opened = *opened;
                if let Some(ignored) = opened.ignored {
                    self.effect(Effect::Log(ignored));
                }
                self.info = opened.info;
                self.sync.synced = opened.synced;
                self.trusted = opened.trusted;
                self.review = Some(Arc::new(opened.review));
                self.blobs.texts.extend(opened.texts);
                self.revisions = overview::summarize(&opened.history, self.settings.policy);
                // The policy in use when the review shows, which the settings list may have
                // changed while it opened.
                self.snapshot = Some(Snapshot {
                    history: opened.history,
                    revision: opened.revision,
                    policy: self.settings.policy,
                });
                self.effect(Effect::Title(self.title()));
                self.phase = Phase::Ready;
                self.refresh_tree();
                self.restore_folders(opened.collapsed);
                self.select_first_file();
                let (status, failed) = opening_status(
                    &self.summary(),
                    opened.found.as_deref(),
                    &self.settings.problems,
                );
                if failed {
                    self.fail(status);
                } else {
                    self.set_status(status);
                }
                self.request_syntax();
                self.start_clock();
            }
            Err(err) => {
                self.effect(Effect::Log(err.clone()));
                self.status = "The review could not be opened.".to_owned();
                self.phase = Phase::Failed(err);
            }
        }
    }

    /// The step every update ends with: saves the folded folders when they changed, quits
    /// once nothing is left to save after the reviewer asked to, and hands over the effects
    /// queued since the last update.
    fn after_update(&mut self) -> Vec<Effect> {
        self.save_folders();
        self.finish_quitting();
        std::mem::take(&mut self.effects)
    }

    /// Whether background work the model started, or an eased scroll, is still running.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.marking
            || !self.marks.is_empty()
            || self.looking_up
            || self.highlighting > 0
            || self.preview.is_some()
            || self.picker.as_ref().is_some_and(picker::Picker::busy)
            || self.refreshing
            || self.folders.busy()
            || self.diff_scroll.animating()
            || self.buffer_scroll.animating()
            || matches!(self.phase, Phase::Loading(_))
    }

    /// The window title: the pull request or range under review.
    #[must_use]
    pub fn title(&self) -> String {
        match &self.info {
            Some(review_core::ReviewInfo::PullRequest(pr)) => {
                format!("#{} {}", pr.number, pr.title)
            }
            _ => self.review.as_ref().map_or_else(
                || "Code review".to_owned(),
                |review| format!("Review of {}", review.key()),
            ),
        }
    }

    /// One line on where the review stands, with how many of the reviewed hunks were approved
    /// automatically as proven formatting-only.
    #[must_use]
    pub(crate) fn summary(&self) -> String {
        let (reviewed, total) = self.totals();
        let revision = self.snapshot.as_ref().map_or_else(String::new, |snapshot| {
            format!("Revision {}", snapshot.revision)
        });
        let auto = match self.auto_approved() {
            0 => String::new(),
            auto => format!(" · {auto} auto-approved"),
        };
        format!(
            "{revision} · {reviewed} of {} reviewed{auto}",
            plural(total, "hunk")
        )
    }

    /// Files whose every unit is reviewed, approved automatically included, and all files,
    /// pending marks included.
    #[must_use]
    pub fn file_totals(&self) -> (usize, usize) {
        self.tree
            .iter()
            .filter(|node| node.kind == NodeKind::File)
            .fold((0, 0), |(done, total), node| {
                let counts = &node.counts;
                (done + usize::from(counts.done() >= counts.units), total + 1)
            })
    }

    /// Reviewed and total units over every file, approved automatically and pending marks
    /// included.
    #[must_use]
    pub fn totals(&self) -> (usize, usize) {
        self.top_counts().fold((0, 0), |(reviewed, total), counts| {
            (reviewed + counts.done(), total + counts.units)
        })
    }

    /// The units over every file approved automatically as proven formatting-only, pending
    /// marks included.
    #[must_use]
    pub fn auto_approved(&self) -> usize {
        self.top_counts().map(|counts| counts.auto_approved).sum()
    }

    /// The counts of the tree's top-level nodes, which together cover every file once.
    fn top_counts(&self) -> impl Iterator<Item = &review_core::Counts> {
        self.tree
            .iter()
            .filter(|node| node.depth == 0)
            .map(|node| &node.counts)
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.failed_at = None;
    }

    /// Says `status` in the status line, as the front end's own news: input it could not
    /// take, for example.
    pub fn notice(&mut self, status: impl Into<String>) {
        self.set_status(status);
    }

    /// Stops the language servers that no lookup is using, as when the front end quits.
    ///
    /// A server a lookup holds stops once that lookup ends and its last holder goes.
    pub fn stop_language_servers(&mut self) {
        if let Some(mut servers) = self.servers.try_lock() {
            servers.clear();
        }
    }

    fn fail(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.failed_at = Some(Instant::now());
    }

    /// The look on screen: the preview of the settings list, or the applied look.
    #[must_use]
    pub fn look(&self) -> &crate::look::Look {
        self.settings.appearance.shown()
    }

    /// The current theme.
    #[must_use]
    pub fn theme(&self) -> &crate::theme::Theme {
        self.look().theme.as_ref()
    }

    /// The shown revision's view.
    #[must_use]
    pub fn view(&self) -> Option<review_core::RevisionView<'_>> {
        self.snapshot.as_ref()?.view()
    }

    /// Which units need the reviewer now: the shown revision's policy, a preview's included,
    /// or the configured one before the review opens.
    #[must_use]
    pub fn policy(&self) -> review_core::ReviewPolicy {
        self.snapshot
            .as_ref()
            .map_or(self.settings.policy, |snapshot| snapshot.policy)
    }

    /// Decides under `policy` which units need the reviewer: the tree, the totals and the
    /// overview follow at once, and the diff once its rows are laid out again.
    fn set_policy(&mut self, policy: review_core::ReviewPolicy) {
        let Some(snapshot) = &mut self.snapshot else {
            return;
        };
        if snapshot.policy == policy {
            return;
        }
        snapshot.policy = policy;
        self.revisions = overview::summarize(&snapshot.history, policy);
        self.refresh_tree();
    }

    /// The area whose keys apply now, and which the front end gives the keyboard.
    #[must_use]
    pub fn area(&self) -> Area {
        let editing = self.typing();
        match (self.overlay, editing, self.mode) {
            (Some(Overlay::Picker), ..) => {
                if self.picker.as_ref().is_some_and(|picker| picker.editing) {
                    Area::Picker
                } else {
                    Area::PickerList
                }
            }
            (Some(Overlay::Settings), ..) => self.settings_area(),
            (Some(_), ..) => Area::Overlay,
            (None, true, _) => Area::Find,
            (None, _, Mode::Tree) => Area::Tree,
            (None, _, Mode::Diff) => Area::Diff,
            (None, _, Mode::Buffer) => Area::Buffer,
        }
    }

    /// Whether the current mode's `/` bar takes typing.
    #[must_use]
    pub fn typing(&self) -> bool {
        match self.mode {
            Mode::Tree => self.filter.as_ref().is_some_and(|filter| filter.editing),
            mode => self
                .find
                .as_ref()
                .is_some_and(|find| find.editing && find.target == mode),
        }
    }

    /// Switches to `mode`, which then owns the keyboard.
    fn enter_mode(&mut self, mode: Mode) {
        // A mode works on the diff of the selected file, never on a preview's header alone.
        self.finish_preview();
        // A search belongs to the mode it searches.
        if self.find.as_ref().is_some_and(|find| find.target != mode) {
            self.find = None;
        }
        self.mode = mode;
        self.selection = None;
    }

    /// Recomputes the file tree from the shown revision and the pending marks.
    fn refresh_tree(&mut self) {
        let Some(view) = self.view() else {
            return;
        };
        let files = state::statuses(&view, &self.pending);
        let cursor = self.cursor_identity();
        self.tree = review_core::tree::file_tree(&files);
        self.update_visible_tree();
        self.restore_cursor(cursor);
    }

    /// Lays out the selected file's diff after something it shows changed (marks, folds, text
    /// or highlights arriving, a new revision), keeping the selected hunk.
    fn rebuild(&mut self) {
        // A file previewed by its header keeps showing only its header: its diff is built at
        // the file's place once the cursor rests.
        if self.preview.is_some() {
            self.show_header();
            return;
        }
        self.build_selected();
    }

    /// Lays out the selected file's diff.
    fn build_selected(&mut self) {
        self.preview = None;
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let Some(view) = snapshot.view() else {
            return;
        };
        let Some(path) = self.selected.clone() else {
            self.rows = Arc::new(Layout::default());
            self.hunk_rows.clear();
            return;
        };
        // The selection is the selected hunk's units; its index follows them into the new rows.
        let selected = self
            .rows
            .hunks
            .get(self.hunk)
            .and_then(|hunk| hunk.first().copied());
        self.rows = Arc::new(self.lay_out(&view, &path));
        self.hunk_rows = hunk_rows(&self.rows);
        if let Some(found) =
            selected.and_then(|unit| self.rows.hunks.iter().position(|hunk| hunk.contains(&unit)))
        {
            self.hunk = found;
        }
        self.hunk = self.hunk.min(self.hunk_rows.len().saturating_sub(1));
        self.refresh_find(Mode::Diff);
    }

    /// Builds the layout of `path`'s diff, with its text and highlights from the caches.
    fn lay_out(&mut self, view: &review_core::RevisionView<'_>, path: &str) -> Layout {
        let path = path.to_owned();
        let units = view.units_of_file(&path);
        let file = units.first().and_then(|&unit| view.file(unit).ok());
        // Text and highlights come from the caches by the blobs of this revision's file, so
        // they always belong to what the units describe.
        self.folds.texts.clear();
        self.folds.syntax.clear();
        if let Some(file) = file {
            let head = file.new.as_ref().map(|side| side.oid.clone());
            let base = file.old.as_ref().map(|side| side.oid.clone());
            if let Some(text) = head.as_ref().and_then(|oid| self.blobs.texts.get(oid)) {
                self.folds.texts.insert(path.clone(), Arc::clone(text));
            }
            let syntax = crate::syntax::FileSyntax {
                head: head
                    .as_ref()
                    .and_then(|oid| self.blobs.syntax.get(oid))
                    .cloned(),
                base: base
                    .as_ref()
                    .and_then(|oid| self.blobs.syntax.get(oid))
                    .cloned(),
            };
            self.folds.syntax.insert(path.clone(), Arc::new(syntax));
        }
        rows::present(rows::build(view, &units, &self.folds), self.presented)
    }

    /// The units of the selected hunk.
    #[must_use]
    pub fn hunk_units(&self) -> Vec<UnitId> {
        self.rows.hunks.get(self.hunk).cloned().unwrap_or_default()
    }

    /// The row the diff's keyboard target starts at: the selected hunk's header.
    #[must_use]
    pub(crate) fn hunk_start(&self) -> Option<usize> {
        self.hunk_rows.get(self.hunk).map(|rows| rows.start)
    }

    /// Opens the cheat sheet or the overview over the window.
    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
    }

    fn close_overlay(&mut self) {
        if self.overlay == Some(Overlay::Settings) {
            self.close_settings();
            return;
        }
        self.overlay = None;
    }

    /// `_`: back to files and diff side by side.
    fn screen_previous(&mut self) {
        self.screen = Screen::Split;
        self.layout.sidebar_visible = true;
    }

    /// `Ctrl+B`: shows or hides the files, and remembers it.
    fn toggle_sidebar(&mut self) {
        self.layout.sidebar_visible = !self.layout.sidebar_visible;
        self.screen = if self.layout.sidebar_visible {
            Screen::Split
        } else {
            Screen::Full
        };
        if !self.layout.sidebar_visible && self.mode == Mode::Tree {
            self.enter_mode(Mode::Diff);
        }
        self.remember_layout();
    }

    /// Whether the files show beside the diff.
    #[must_use]
    pub fn sidebar_shown(&self) -> bool {
        self.screen == Screen::Split && self.layout.sidebar_visible
    }

    /// Sets the files pane's width, in the front end's units, as its divider is dragged; see
    /// [`Self::remember_layout`].
    pub fn set_sidebar_width(&mut self, width: f32) {
        self.layout.sidebar = width;
    }

    /// Writes the pane width to the configuration file, for the next launch: the front end's
    /// own width, and whether the files show.
    pub fn remember_layout(&mut self) {
        let Some(path) = self.settings.config_path.clone() else {
            return;
        };
        let layout = self.layout;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a pane width in logical pixels or cells, clamped far below i64's range"
        )]
        let sidebar = layout.sidebar.round() as i64;
        let values = [
            crate::config::Remembered {
                table: "layout",
                key: self.settings.front_end.sidebar_key(),
                value: sidebar.into(),
            },
            crate::config::Remembered {
                table: "layout",
                key: "sidebar_visible",
                value: layout.sidebar_visible.into(),
            },
        ];
        if let Err(err) = crate::config::remember(&path, &values) {
            self.fail(format!("Could not remember the pane width: {err}"));
        }
    }

    /// Reads, in the background, the head text of the shown revision's files that are not
    /// cached yet, then highlights both versions of every file.
    fn request_syntax(&mut self) {
        let (Some(review), Some(snapshot)) = (self.review.clone(), self.snapshot.clone()) else {
            return;
        };
        let Some(view) = snapshot.view() else {
            return;
        };
        let Some(analysis) = view.analysis() else {
            return;
        };
        let mut wanted: Vec<(String, ObjectId)> = Vec::new();
        for file in &analysis.files {
            for side in [file.new.as_ref(), file.old.as_ref()]
                .into_iter()
                .flatten()
                .filter(|side| is_regular(side))
                .filter(|side| review_core::syntax::Language::from_path(&side.path).is_some())
            {
                if !self.blobs.syntax.contains_key(&side.oid)
                    && self.requested.claim(&side.oid, state::Product::Syntax)
                {
                    wanted.push((side.path.clone(), side.oid.clone()));
                }
            }
        }
        let texts_wanted: Vec<ObjectId> = analysis
            .files
            .iter()
            .filter_map(|file| file.new.as_ref())
            .filter(|side| is_regular(side))
            .filter(|side| !self.blobs.texts.contains_key(&side.oid))
            .filter(|side| self.requested.claim(&side.oid, state::Product::Text))
            .map(|side| side.oid.clone())
            .collect();
        if wanted.is_empty() && texts_wanted.is_empty() {
            return;
        }
        let asked: Vec<(ObjectId, state::Product)> = wanted
            .iter()
            .map(|(_, oid)| (oid.clone(), state::Product::Syntax))
            .chain(
                texts_wanted
                    .iter()
                    .map(|oid| (oid.clone(), state::Product::Text)),
            )
            .collect();
        self.highlighting += 1;
        self.spawn(Subject::Always(Because::Content), move || {
            let mut texts = HashMap::new();
            let mut syntax = HashMap::new();
            if let Ok(mut reader) = review.repo().blob_reader() {
                for oid in texts_wanted {
                    if let Ok(review_core::git::Blob::Content(bytes)) = reader.read(&oid)
                        && !bytes.contains(&0)
                    {
                        texts.insert(
                            oid,
                            Arc::new(rows::FullText::new(&String::from_utf8_lossy(&bytes))),
                        );
                    }
                }
                for (path, oid) in wanted {
                    if let Ok(review_core::git::Blob::Content(bytes)) = reader.read(&oid)
                        && let Ok(text) = std::str::from_utf8(&bytes)
                    {
                        syntax.insert(oid, Arc::new(crate::syntax::analyze(&path, text)));
                    }
                }
            }
            Outcome::Highlighted(Highlighted {
                asked,
                texts,
                syntax,
            })
        });
    }

    /// Takes in file texts and highlights read in the background.
    fn highlighted(&mut self, highlighted: Highlighted) {
        let Highlighted { texts, syntax, .. } = highlighted;
        self.blobs.texts.extend(texts);
        self.blobs.syntax.extend(syntax);
        // A late answer for a revision no longer shown is not kept.
        if let Some(view) = self.snapshot.as_ref().and_then(state::Snapshot::view) {
            self.blobs.retain_for(&view);
        }
        self.rebuild();
        self.refresh_buffer_syntax();
    }
}

/// What opening a review produced.
pub(super) struct Opened {
    review: Review,
    /// The folded folders saved with the review.
    collapsed: Vec<String>,
    /// Why the saved folded folders were ignored, for the developer's log.
    ignored: Option<String>,
    found: Option<String>,
    trusted: bool,
    info: Option<review_core::ReviewInfo>,
    /// When the review last synced with its target.
    synced: Option<review_core::Synced>,
    history: Arc<History>,
    /// The revision to show first, the current one.
    revision: RevisionNumber,
    texts: HashMap<ObjectId, Arc<rows::FullText>>,
}

/// Opens the review with `opener` and reads what the window shows first; runs in the
/// background.
fn open(opener: Opener) -> Result<Opened, String> {
    let Loaded {
        review,
        history,
        found,
    } = opener()?;
    let trust = review.trust().map_err(|err| err.to_string())?;
    let info = review.info().ok().flatten();
    let synced = review.synced().ok().flatten();
    let revision = history.current().ok_or("the review has no revision yet")?;
    let history = Arc::new(history);
    let texts = read_texts(&review, &history, revision);
    let (collapsed, ignored) = match crate::folders::load(&review) {
        Ok(collapsed) => (collapsed, None),
        Err(ignored) => (Vec::new(), Some(ignored)),
    };
    Ok(Opened {
        review,
        collapsed,
        ignored,
        found,
        trusted: trust.trusted,
        info,
        synced,
        history,
        revision,
        texts,
    })
}

/// The status line when a review opens, and whether it reports a failure: a configuration
/// problem first, where the reviewer looks rather than only on stderr; then how the review was
/// found, as for the current branch's pull request; then the review's progress.
fn opening_status(summary: &str, found: Option<&str>, problems: &[String]) -> (String, bool) {
    let mut parts = Vec::new();
    if let Some(problem) = problems.first() {
        let more = match problems.len() - 1 {
            0 => String::new(),
            more => format!(" ({} on stderr)", plural(more, "more problem")),
        };
        parts.push(format!("Configuration: {problem}{more}"));
    }
    if let Some(found) = found {
        let mut letters = found.chars();
        parts.extend(
            letters
                .next()
                .map(|first| first.to_uppercase().chain(letters).collect::<String>()),
        );
    }
    parts.push(summary.to_owned());
    (parts.join(" · "), !problems.is_empty())
}

/// Reads the head text of every changed text file of `revision`, by blob, for context lines.
fn read_texts(
    review: &Review,
    history: &History,
    revision: RevisionNumber,
) -> HashMap<ObjectId, Arc<rows::FullText>> {
    let mut texts = HashMap::new();
    let Some(analysis) = history
        .view(revision)
        .ok()
        .and_then(|view| view.analysis().cloned())
    else {
        return texts;
    };
    let Ok(mut reader) = review.repo().blob_reader() else {
        return texts;
    };
    for side in analysis
        .files
        .iter()
        .filter_map(|file| file.new.as_ref())
        .filter(|side| is_regular(side))
    {
        if let Ok(review_core::git::Blob::Content(bytes)) = reader.read(&side.oid)
            && !bytes.contains(&0)
        {
            texts.insert(
                side.oid.clone(),
                Arc::new(rows::FullText::new(&String::from_utf8_lossy(&bytes))),
            );
        }
    }
    texts
}

/// Whether a tree entry is a regular file, whose blob is text to show; a symlink's blob is its
/// target and a submodule's is a commit, which the entry evidence shows instead.
fn is_regular(side: &review_core::analysis::EntrySide) -> bool {
    side.mode.entry_type() == review_core::git::EntryType::File
}

/// The row range of each display hunk: the rows that carry its index, which are contiguous.
fn hunk_rows(layout: &Layout) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = vec![0..0; layout.hunks.len()];
    for (index, row) in layout.rows.iter().enumerate() {
        if let Some(range) = row.hunk.and_then(|hunk| ranges.get_mut(hunk)) {
            if range.start == range.end {
                *range = index..index + 1;
            } else {
                range.end = index + 1;
            }
        }
    }
    ranges
}

/// The paths of the files in `tree`, in tree order.
#[must_use]
pub fn file_paths(tree: &[TreeNode]) -> Vec<&str> {
    tree.iter()
        .filter(|node| node.kind == NodeKind::File)
        .map(|node| node.path.as_str())
        .collect()
}

/// `count` followed by `noun`, pluralized.
#[must_use]
pub fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

#[cfg(test)]
mod opening_tests {
    use super::*;

    /// The opening status names a configuration problem first, as a failure, then how the
    /// review was found, then its progress.
    #[test]
    fn the_opening_status_says_what_matters_first() {
        let summary = "Revision 1 · 7 of 27 hunks reviewed";
        assert_eq!(
            opening_status(summary, None, &[]),
            (summary.to_owned(), false)
        );
        let found = "reviewing pull request #12 from feature (https://example.com/pull/12)";
        assert_eq!(
            opening_status(summary, Some(found), &[]),
            (
                format!(
                    "Reviewing pull request #12 from feature (https://example.com/pull/12) · \
                     {summary}"
                ),
                false
            )
        );
        let problems = ["there is no theme \"x\"".to_owned(), "another".to_owned()];
        let (status, failed) = opening_status(summary, Some(found), &problems);
        assert!(failed);
        assert!(
            status.starts_with(
                "Configuration: there is no theme \"x\" (1 more problem on stderr) · \
                 Reviewing pull request #12"
            ),
            "{status}"
        );
        assert!(status.ends_with(summary), "{status}");
    }
}
