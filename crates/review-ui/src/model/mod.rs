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
use std::sync::atomic::{AtomicBool, Ordering};
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
mod highlights;
mod layout;
mod live;
mod marks;
pub mod overview;
pub mod picker;
pub mod present;
mod pulls;
mod quit;
mod search;
mod select;
pub mod settings;
pub mod sync;
#[cfg(test)]
mod tests;
mod tree;

pub use actions::Action;
use effects::{Because, Highlighted, Outcome, Subject, TextsRead};
pub use effects::{Completion, Driver, Effect, Job, SheetStep, Updated};
pub use fields::{Field, FieldState};
pub use live::{Following, abbreviated, branch_and_base};
pub use pulls::Found;
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

/// View settings from the configuration file, and the look as the reviewer changes it.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Where the configuration file is, for remembering choices made in the app.
    pub config_path: Option<PathBuf>,
    /// The code size the window opens with, in logical pixels, within [`ZOOM_RANGE`].
    pub code_size: f32,
    /// Unchanged lines shown around each hunk.
    pub context: usize,
    /// Whether the files pane and the file header shorten a folder chain's middle folders.
    pub shorten_chains: bool,
    /// What the mouse may do.
    pub mouse: crate::config::Mouse,
    /// Whether approved hunks fold, as the configuration sets it.
    pub approved: crate::config::Approved,
    /// Which units still need the reviewer, as the configuration sets it: whether proven
    /// formatting-only units count as approved without a mark.
    pub policy: review_core::ReviewPolicy,
    /// Whether a language server may start for go to definition, as `[lsp] enabled` sets it.
    pub language_servers: bool,
    /// Whether a review of the working tree follows it as it changes, as `[review] live` sets
    /// it.
    pub live: bool,
    /// The removed-line background, as `0xRRGGBB`, when the configuration overrides the
    /// theme's.
    pub removed: Option<u32>,
    /// The added-line background, likewise.
    pub added: Option<u32>,
    /// The look applied, and the one previewed over it while the settings list is open.
    pub appearance: crate::look::Appearance,
    /// Every theme the settings list offers.
    pub themes: Vec<crate::theme::Theme>,
    /// Every diff style, in the order `S` cycles them.
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
    /// The width, the front end's own: fitted to the tree, or set in its units, logical pixels
    /// in review-gui and cells in review-tui; see [`FrontEnd::sidebar_key`].
    pub sidebar: crate::panes::SidebarWidth,
    /// Whether the files show, which every front end shares.
    pub sidebar_visible: bool,
}

/// An opened review with its history.
pub struct Loaded {
    /// The review.
    pub review: Review,
    /// Its revisions.
    pub history: History,
    /// How the review was found, such as following the checkout's working tree, which the
    /// status line says when the review opens.
    pub found: Option<String>,
    /// For a review of the checkout, the session it opened under and what its opening capture
    /// saw; `None` for any other review, which starts a session of its own when it follows the
    /// working tree.
    pub following: Option<Following>,
}

/// The grant of trust that lets tools run on the checkout's snapshot on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    /// A grant for exactly this snapshot, which ends when the code changes.
    Snapshot,
    /// A grant for the whole checkout, until it is revoked.
    Checkout,
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
    /// The question whether to trust the checkout's code, before a tool runs it.
    Trust,
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
    /// The selected hunk's first unit: a hunk's index depends on the view, its units do not.
    unit: Option<UnitId>,
    top: present::Anchor,
    /// What the top row showed, to find it again in rows laid out in another view.
    key: Option<present::RowKey>,
    /// The view the rows were laid out in when the place was taken.
    view: crate::config::DiffView,
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

/// Which language server: its program and the worktree it runs in.
type ServerKey = (&'static str, PathBuf);

/// Running language servers by program and worktree; the map's own lock is held only to find,
/// launch or stop them, never across a handshake or a request.
type Servers = Arc<Mutex<HashMap<ServerKey, Running>>>;

/// A running language server, locked while a lookup uses it, with what stops it without that
/// lock.
///
/// It stops as it leaves the map, however it leaves: a lookup still holding the server sees
/// its wait fail rather than keeping it running unlisted, where no revocation finds it.
struct Running {
    server: Arc<Mutex<LanguageServer>>,
    stopper: crate::lsp::Stopper,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stopper.stop();
    }
}

impl Running {
    /// `server`, with its stopper.
    fn new(server: LanguageServer) -> Self {
        Self {
            stopper: server.stopper(),
            server: Arc::new(Mutex::new(server)),
        }
    }
}

/// File text and highlights by blob, filled in the background; content never changes under an
/// object ID, so these never go stale.
#[derive(Default)]
pub(crate) struct Blobs {
    texts: HashMap<ObjectId, Arc<rows::FullText>>,
    /// The syntax each file version resolved to, from its path and text.
    resolved: HashMap<state::SideKey, crate::syntax::Resolved>,
    /// Highlights by blob, then by syntax, as the same blob may be highlighted as two syntaxes.
    syntax: HashMap<ObjectId, HashMap<crate::syntax::Resolved, Arc<SideSyntax>>>,
}

impl Blobs {
    /// Keeps only the blobs `view` shows, so a long review's earlier revisions do not stay in
    /// memory for the session.
    fn retain_for(&mut self, view: &review_core::RevisionView<'_>) {
        let sides: HashSet<state::SideKey> = shown_sides(view).collect();
        let shown: HashSet<&ObjectId> = sides.iter().map(|side| &side.oid).collect();
        self.texts.retain(|oid, _| shown.contains(oid));
        self.syntax.retain(|oid, _| shown.contains(oid));
        self.resolved.retain(|side, _| sides.contains(side));
    }

    /// The highlights of the file version `side`, once its syntax is resolved and highlighted.
    fn side_syntax(&self, side: &review_core::analysis::EntrySide) -> Option<Arc<SideSyntax>> {
        self.syntax_of(&state::SideKey {
            oid: side.oid.clone(),
            path: side.path.clone(),
        })
    }

    /// [`Self::side_syntax`] by the version's key.
    fn syntax_of(&self, side: &state::SideKey) -> Option<Arc<SideSyntax>> {
        let resolved = self.resolved.get(side)?;
        self.syntax.get(&side.oid)?.get(resolved).cloned()
    }

    /// The highlights already made for the blobs of `sides`, which a version that resolves to
    /// the same syntax reuses.
    fn known(&self, sides: &[state::SideKey]) -> HashSet<state::SyntaxKey> {
        sides
            .iter()
            .filter_map(|side| Some((&side.oid, self.syntax.get(&side.oid)?)))
            .flat_map(|(oid, made)| {
                made.keys().map(|resolved| state::SyntaxKey {
                    oid: oid.clone(),
                    resolved: resolved.clone(),
                })
            })
            .collect()
    }

    /// Whether the file version `side` still needs highlighting: its syntax is not resolved
    /// yet, or resolved to highlights not made yet.
    fn needs_highlighting(&self, side: &state::SideKey) -> bool {
        match self.resolved.get(side) {
            None => true,
            Some(resolved) if resolved.syntax == crate::syntax::Syntax::Plain => false,
            Some(resolved) => !self
                .syntax
                .get(&side.oid)
                .is_some_and(|made| made.contains_key(resolved)),
        }
    }
}

/// Every file version `view` shows, by blob and path.
fn shown_sides<'a>(
    view: &'a review_core::RevisionView<'_>,
) -> impl Iterator<Item = state::SideKey> + 'a {
    view.analysis()
        .into_iter()
        .flat_map(|analysis| &analysis.files)
        .flat_map(|file| [file.old.as_ref(), file.new.as_ref()])
        .flatten()
        .map(|side| state::SideKey {
            oid: side.oid.clone(),
            path: side.path.clone(),
        })
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
    /// Blobs whose text is being read right now, so none is requested twice at once; an entry
    /// goes when its read completes, so a blob shown again after pruning is requested again.
    requested: state::Outstanding,
    /// What the review is about.
    pub info: Option<review_core::ReviewInfo>,
    /// Every revision, for the overview.
    pub revisions: Vec<overview::RevisionSummary>,
    /// The file tree with pending marks applied.
    pub tree: Vec<TreeNode>,
    /// The cells the tree's widest row takes, from [`crate::paint::files::tree_cells`].
    tree_cells: usize,
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
    /// The generation of the snapshot on screen, which changes whenever what it shows does, so
    /// work done for one snapshot never lands on another, even under the same revision number,
    /// as successive live captures of the working tree have.
    shown: u64,
    /// The version of the newest history installed; an older one knows less, such as a mark
    /// saved since, and is never installed over it.
    version: u64,
    /// Following the working tree, for a live review of it.
    live: Option<live::Live>,
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
    /// Whether the diff is in scroll mode, where the hunk keys scroll a line.
    pub scroll_mode: bool,
    /// The first unit of the hunk selected when scroll mode began, which Esc goes back to.
    scrolled_from: Option<UnitId>,
    /// When the status line last reported a failure, for tinting it.
    pub failed_at: Option<Instant>,
    /// Whether a definition lookup runs.
    pub(crate) looking_up: bool,
    /// Background reads of file text still running.
    pub(crate) highlighting: usize,
    /// The shown revision's highlighting in the background.
    highlights: highlights::Highlights,
    /// Whether language servers may run on the reviewed code; see [`Review::trust`].
    pub trusted: bool,
    /// For a review of the checkout, the grant that lets tools run on the snapshot on screen,
    /// if one does.
    pub grant: Option<Grant>,
    /// The head of the snapshot a confirming second press would trust, while trusting awaits
    /// one; another snapshot showing meanwhile drops it.
    trust_pending: Option<ObjectId>,
    /// Whether go to definition waits for the trust it asked for.
    trust_resumes: bool,
    /// The decisions on trust made in the window, counted, so a grant read in the background
    /// before the latest decision is read again rather than shown.
    trust_decisions: u64,
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
            tree_cells: 0,
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
            shown: 0,
            version: 0,
            live: None,
            marks: VecDeque::new(),
            marking: false,
            pending: HashMap::new(),
            refreshing: false,
            sync: Sync::default(),
            quitting: Quitting::No,
            quit_said: String::new(),
            layout: settings.layout,
            status: "Opening the review…".to_owned(),
            scroll_mode: false,
            scrolled_from: None,
            failed_at: None,
            looking_up: false,
            highlighting: 0,
            highlights: highlights::Highlights::new(highlights::Highlights::machine_workers()),
            trusted: false,
            grant: None,
            trust_pending: None,
            trust_resumes: false,
            trust_decisions: 0,
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
                if let Some(granted) = opened.granted {
                    self.set_grant(granted.grant);
                }
                self.review = Some(Arc::new(opened.review));
                self.blobs.texts.extend(opened.texts);
                self.revisions = overview::summarize(&opened.history, self.settings.policy);
                self.shown += 1;
                self.version = opened.history.version();
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
                self.restore_view_state(opened.view_state);
                let found = match &opened.following {
                    Some(following) => following.describe(),
                    None => opened.found,
                };
                let (status, failed) =
                    opening_status(&self.summary(), found.as_deref(), &self.settings.problems);
                if failed {
                    self.fail(status);
                } else {
                    self.set_status(status);
                }
                self.request_syntax();
                self.start_clock();
                self.start_live(opened.following);
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
        self.show_waiting_capture();
        self.save_view_state();
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
            || self.highlights.running()
            || self.picker.as_ref().is_some_and(picker::Picker::busy)
            || self.refreshing
            || self.live.as_ref().is_some_and(live::Live::capturing)
            || self.folders.busy()
            || self.diff_scroll.animating()
            || self.buffer_scroll.animating()
            || matches!(self.phase, Phase::Loading(_))
    }

    /// The window title: the pull request or range under review, or for the checkout, its
    /// directory with the branch and its pull request, such as `code-review · feat/x #12`.
    #[must_use]
    pub fn title(&self) -> String {
        if let Some(title) = self.checkout_title() {
            return title;
        }
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

    /// The title of a review of the checkout, from its directory and what the capture on
    /// screen was of.
    fn checkout_title(&self) -> Option<String> {
        let review = self.review.as_ref()?;
        if !matches!(review.target(), review_core::ReviewTarget::Checkout { .. }) {
            return None;
        }
        let directory = review
            .repo()
            .work_tree()
            .and_then(|tree| tree.file_name())
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let Some(subject) = self.live.as_ref().and_then(|live| live.subject()) else {
            return Some(directory);
        };
        let head = match (&subject.branch, &subject.head) {
            (Some(branch), _) => branch.clone(),
            (None, Some(head)) => format!("detached {}", live::abbreviated(head)),
            (None, None) => "detached".to_owned(),
        };
        // The pull request the capture on screen resolved its base with.
        let number = subject
            .pull_request
            .as_ref()
            .map_or_else(String::new, |pull_request| {
                format!(" #{}", pull_request.number)
            });
        Some(format!("{directory} · {head}{number}"))
    }

    /// Takes in the grant that applies to the checkout's snapshot on screen: tools may run on
    /// it exactly while one does.
    fn set_grant(&mut self, grant: Option<Grant>) {
        self.grant = grant;
        self.set_trusted(grant.is_some());
    }

    /// Takes in whether tools may run on the snapshot on screen: once they may not, as when
    /// another window revoked trust, the language servers stop, as a revocation here stops
    /// them.
    fn set_trusted(&mut self, trusted: bool) {
        if self.trusted && !trusted {
            // Each server stops as it leaves the map.
            self.servers.lock().clear();
        }
        self.trusted = trusted;
    }

    /// For a review trusted one snapshot at a time, reads again whether the snapshot on screen
    /// is trusted, as when another one shows.
    fn read_snapshot_trust(&mut self) {
        // A confirmation asked for on another snapshot does not carry over.
        self.trust_pending = None;
        let (Some(review), Some(snapshot)) = (&self.review, &self.snapshot) else {
            return;
        };
        if review.trusted_per_snapshot() {
            let trusted = snapshot_trusted(review, &snapshot.history, snapshot.revision);
            self.set_trusted(trusted);
        }
    }

    /// Whether the review is of the checkout, whatever it holds.
    #[must_use]
    pub fn is_checkout(&self) -> bool {
        self.review.as_ref().is_some_and(|review| {
            matches!(review.target(), review_core::ReviewTarget::Checkout { .. })
        })
    }

    /// For a review that follows the working tree, what the capture on screen was of, whether
    /// it may miss a change, and what keeps it from updating; see [`LiveState`].
    #[must_use]
    pub fn live_state(&self) -> Option<LiveState<'_>> {
        let live = self.live.as_ref()?;
        Some(LiveState {
            subject: live.subject(),
            pull_request: live.pulls.current(),
            lookup: live.pulls.unavailable(),
            looking_up: live.pulls.running(),
            lookup_failed: live.pulls.failed(),
            filtered: live.filtered(),
            updating: live.updating(),
            busy: live.busy(),
            replaced: live.replaced(),
        })
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
        // A search belongs to the mode it searches.
        if self.find.as_ref().is_some_and(|find| find.target != mode) {
            self.find = None;
        }
        self.mode = mode;
        self.selection = None;
    }

    /// Recomputes the file tree from the shown revision and the pending marks.
    #[tracing::instrument(skip_all)]
    fn refresh_tree(&mut self) {
        let Some(view) = self.view() else {
            return;
        };
        let files = state::statuses(&view, &self.pending);
        let cursor = self.cursor_identity();
        self.tree = review_core::tree::file_tree(&files);
        self.tree_cells = crate::paint::files::tree_cells(&self.tree, self.settings.shorten_chains);
        self.update_visible_tree();
        self.restore_cursor(cursor);
    }

    /// Lays out the selected file's diff after something it shows changed (marks, folds, text
    /// or highlights arriving, a new revision), keeping the selected hunk.
    fn rebuild(&mut self) {
        self.build_selected();
    }

    /// Lays out the selected file's diff.
    #[tracing::instrument(skip_all)]
    fn build_selected(&mut self) {
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
    #[tracing::instrument(skip_all, fields(%path))]
    fn lay_out(&mut self, view: &review_core::RevisionView<'_>, path: &str) -> Layout {
        let path = path.to_owned();
        let units = view.units_of_file(&path);
        let file = units.first().and_then(|&unit| view.file(unit).ok());
        // Text and highlights come from the caches by the blobs of this revision's file, so
        // they always belong to what the units describe.
        self.folds.texts.clear();
        self.folds.syntax.clear();
        self.folds.view = self.presented.view;
        self.folds.pending.clone_from(&self.pending);
        if let Some(file) = file {
            if let Some(text) = file
                .new
                .as_ref()
                .and_then(|side| self.blobs.texts.get(&side.oid))
            {
                self.folds.texts.insert(path.clone(), Arc::clone(text));
            }
            let syntax = crate::syntax::FileSyntax {
                head: file
                    .new
                    .as_ref()
                    .and_then(|side| self.blobs.side_syntax(side)),
                base: file
                    .old
                    .as_ref()
                    .and_then(|side| self.blobs.side_syntax(side)),
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
        if !self.settings.mouse.navigate {
            return;
        }
        self.layout.sidebar = crate::panes::SidebarWidth::Set(width);
    }

    /// The cells the tree's widest row takes from its start to the end of its name, with every
    /// folder open, which a fitted files pane follows; see [`crate::paint::files::tree_cells`].
    #[must_use]
    pub fn tree_cells(&self) -> usize {
        self.tree_cells
    }

    /// Writes the pane width to the configuration file, for the next launch: the front end's
    /// own width, or `"fit"`, and whether the files show.
    pub fn remember_layout(&mut self) {
        let Some(path) = self.settings.config_path.clone() else {
            return;
        };
        let layout = self.layout;
        let sidebar = match layout.sidebar {
            crate::panes::SidebarWidth::Fit => "fit".into(),
            crate::panes::SidebarWidth::Set(width) => {
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a pane width in logical pixels or cells, clamped far below i64's range"
                )]
                let width = width.round() as i64;
                width.into()
            }
        };
        let values = [
            crate::config::Remembered {
                table: "layout",
                key: self.settings.front_end.sidebar_key(),
                value: sidebar,
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
    /// cached yet, and highlights the files that need it.
    fn request_syntax(&mut self) {
        self.request_texts();
        self.pump_highlights();
    }

    /// Reads the head text of the shown revision's files that are not cached yet, for context
    /// lines and the file buffer.
    fn request_texts(&mut self) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let Some(view) = self.view() else {
            return;
        };
        let Some(analysis) = view.analysis() else {
            return;
        };
        let wanted: Vec<ObjectId> = analysis
            .files
            .iter()
            .filter_map(|file| file.new.as_ref())
            .filter(|side| is_regular(side))
            .filter(|side| !self.blobs.texts.contains_key(&side.oid))
            .map(|side| side.oid.clone())
            .collect();
        let wanted: Vec<ObjectId> = wanted
            .into_iter()
            .filter(|oid| self.requested.claim(oid))
            .collect();
        if wanted.is_empty() {
            return;
        }
        let asked = wanted.clone();
        self.highlighting += 1;
        self.spawn(Subject::Always(Because::Content), move || {
            let _reading = tracing::info_span!("read_texts", blobs = wanted.len()).entered();
            let mut texts = HashMap::new();
            if let Ok(mut reader) = review.repo().blob_reader() {
                for oid in wanted {
                    if let Ok(review_core::git::Blob::Content(bytes)) = reader.read(&oid)
                        && !bytes.contains(&0)
                    {
                        texts.insert(
                            oid,
                            Arc::new(rows::FullText::new(&String::from_utf8_lossy(&bytes))),
                        );
                    }
                }
            }
            Outcome::TextsRead(TextsRead { asked, texts })
        });
    }

    /// Starts the highlighting the shown revision needs now, the selected file first; see
    /// [`highlights`].
    fn pump_highlights(&mut self) {
        let Some(review) = self.review.clone() else {
            return;
        };
        if self.highlights.indexed_for() != Some(self.shown) {
            let Some(view) = self.snapshot.as_ref().and_then(Snapshot::view) else {
                return;
            };
            let files = highlight_files(&view);
            self.blobs.retain_for(&view);
            let blobs = &self.blobs;
            self.highlights
                .reindex(self.shown, files, |side| blobs.needs_highlighting(side));
        }
        let blobs = &self.blobs;
        let starts = self.highlights.pump(self.selected.as_deref(), |side| {
            blobs.needs_highlighting(side)
        });
        for start in starts {
            let known = self.blobs.known(&start.sides);
            let review = Arc::clone(&review);
            self.spawn(Subject::Always(Because::Content), move || {
                Outcome::Highlighted(highlight_sides(
                    &review,
                    start.id,
                    start.sides,
                    &known,
                    start.cancel.as_deref(),
                ))
            });
        }
    }

    /// Takes in file texts read in the background.
    fn texts_read(&mut self, read: TextsRead) {
        self.blobs.texts.extend(read.texts);
        // A late answer for a revision no longer shown is not kept.
        if let Some(view) = self.snapshot.as_ref().and_then(state::Snapshot::view) {
            self.blobs.retain_for(&view);
        }
        self.rebuild();
        self.refresh_buffer_syntax();
    }

    /// The versions of the selected file, base and head, each under its own path: a renamed
    /// file's base is under its old path, which is not the one the tree shows.
    fn selected_sides(&self) -> Vec<state::SideKey> {
        self.selected
            .as_deref()
            .map(|path| self.highlights.sides_at(path))
            .unwrap_or_default()
    }

    /// Takes in what a highlight job made, keeps it for the versions the shown revision has,
    /// and starts what runs next.
    ///
    /// Every highlight job's completion lands here, so this is where each job ends; a cancelled
    /// job's highlights may be partial and are not kept, so its versions are highlighted again.
    fn highlighted(&mut self, highlighted: Highlighted) {
        let Highlighted {
            job,
            resolved,
            syntax,
            cancelled,
        } = highlighted;
        let sides = self.highlights.retire(job);
        if !cancelled {
            let resolved: HashMap<state::SideKey, crate::syntax::Resolved> = resolved
                .into_iter()
                .filter(|(side, _)| self.highlights.shows(side))
                .collect();
            let selected = self.selected_sides();
            let shown = resolved.keys().any(|side| selected.contains(side));
            let blobs: HashSet<&ObjectId> = resolved.keys().map(|side| &side.oid).collect();
            for (key, highlights) in syntax {
                if blobs.contains(&key.oid) {
                    self.blobs
                        .syntax
                        .entry(key.oid)
                        .or_default()
                        .insert(key.resolved, highlights);
                }
            }
            self.blobs.resolved.extend(resolved);
            if shown {
                self.rebuild();
            }
            self.refresh_buffer_syntax();
        }
        let blobs = &self.blobs;
        self.highlights
            .refresh(&sides, |side| blobs.needs_highlighting(side));
        self.pump_highlights();
    }
}

/// The versions of `view`'s files that can be highlighted, in review order.
fn highlight_files(view: &review_core::RevisionView<'_>) -> Vec<highlights::FileSides> {
    let key = |side: &review_core::analysis::EntrySide| state::SideKey {
        oid: side.oid.clone(),
        path: side.path.clone(),
    };
    view.analysis()
        .into_iter()
        .flat_map(|analysis| &analysis.files)
        .map(|file| highlights::FileSides {
            old: file.old.as_ref().filter(|side| is_regular(side)).map(key),
            new: file.new.as_ref().filter(|side| is_regular(side)).map(key),
        })
        .collect()
}

/// Reads and highlights the file versions `sides` as job `job` of [`Model::pump_highlights`];
/// runs in the background.
///
/// A version that is binary, oversized, not UTF-8 or unreadable resolves to plain.
/// Once `cancel` is set the job stops where it is and says it was cancelled, since what it made
/// by then may be partial.
#[tracing::instrument(skip_all, fields(sides = sides.len()))]
fn highlight_sides(
    review: &Review,
    job: highlights::JobId,
    sides: Vec<state::SideKey>,
    known: &HashSet<state::SyntaxKey>,
    cancel: Option<&AtomicBool>,
) -> Highlighted {
    let cancelled = || cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed));
    let mut resolved = HashMap::new();
    let mut syntax = HashMap::new();
    let mut reader = review.repo().blob_reader().ok();
    for side in sides {
        if cancelled() {
            break;
        }
        let read = reader
            .as_mut()
            .and_then(|reader| reader.read(&side.oid).ok());
        let side_resolved = match &read {
            Some(review_core::git::Blob::Content(bytes)) if !bytes.contains(&0) => {
                std::str::from_utf8(bytes).ok().map(|text| {
                    let side_resolved = crate::syntax::resolve(&side.path, text);
                    let key = state::SyntaxKey {
                        oid: side.oid.clone(),
                        resolved: side_resolved.clone(),
                    };
                    let made = known.contains(&key) || syntax.contains_key(&key);
                    if side_resolved.syntax != crate::syntax::Syntax::Plain && !made {
                        let highlights = crate::syntax::highlight(&side_resolved, text, cancel);
                        syntax.insert(key, Arc::new(highlights));
                    }
                    side_resolved
                })
            }
            _ => None,
        };
        resolved.insert(
            side,
            side_resolved.unwrap_or(crate::syntax::Resolved::PLAIN),
        );
    }
    Highlighted {
        job,
        resolved,
        syntax,
        cancelled: cancelled(),
    }
}

/// What a review that follows the working tree shows besides its diff.
#[derive(Debug, Clone, Copy)]
pub struct LiveState<'a> {
    /// What the capture on screen was of: the branch, `HEAD`, the base, and the pull request
    /// it resolved the base from; `None` for a review that does not observe one.
    pub subject: Option<&'a review_core::Subject>,
    /// The branch's pull request as last looked up, if it was.
    pub pull_request: Option<&'a Found>,
    /// Why the pull request is not looked up, if it is not.
    pub lookup: Option<&'a str>,
    /// Whether a lookup of the pull request runs.
    pub looking_up: bool,
    /// Why the last lookup of the pull request failed, if it did.
    pub lookup_failed: Option<&'a str>,
    /// The paths the capture kept at their staged version, under a filter driver.
    pub filtered: &'a [String],
    /// Whether the view may miss a change and takes no marks until it is captured again.
    pub updating: bool,
    /// What git is busy with in the checkout, which keeps the view on screen.
    pub busy: Option<review_core::Busy>,
    /// Whether the checkout is another clone now, so nothing is captured any more.
    pub replaced: bool,
}

/// What opening a review produced.
pub(super) struct Opened {
    review: Review,
    /// The folded folders and the place saved with the review.
    view_state: crate::folders::ViewState,
    /// Why the saved view state was ignored, for the developer's log.
    ignored: Option<String>,
    found: Option<String>,
    trusted: bool,
    /// For a review of the checkout, the grant that applies to its opening capture.
    granted: Option<live::Granted>,
    /// For a review of the checkout, what its opening capture saw.
    following: Option<live::Opening>,
    info: Option<review_core::ReviewInfo>,
    /// When the review last synced with its target.
    synced: Option<review_core::Synced>,
    history: Arc<History>,
    /// The revision to show first, the current one.
    revision: RevisionNumber,
    texts: HashMap<ObjectId, Arc<rows::FullText>>,
}

/// Whether the reviewer trusts the snapshot of `history`'s revision `revision`, the head it
/// reviews, for a review trusted one snapshot at a time; see [`Review::trust_for`].
pub(super) fn snapshot_trusted(
    review: &Review,
    history: &History,
    revision: RevisionNumber,
) -> bool {
    let tree = history
        .view(revision)
        .ok()
        .and_then(|view| view.record().map(|record| record.head.clone()))
        .and_then(|head| review.repo().tree_of(&head).ok());
    tree.is_some_and(|tree| review.trust_for(&tree).is_ok_and(|trust| trust.trusted))
}

/// Opens the review with `opener` and reads what the window shows first; runs in the
/// background.
fn open(opener: Opener) -> Result<Opened, String> {
    let Loaded {
        review,
        history,
        found,
        following,
    } = opener()?;
    let revision = history.current().ok_or("the review has no revision yet")?;
    let trusted = if review.trusted_per_snapshot() {
        snapshot_trusted(&review, &history, revision)
    } else {
        review.trust().map_err(|err| err.to_string())?.trusted
    };
    let (following, granted) = match following {
        Some(following) => {
            let (opening, granted) = live::Opening::read(&review, &history, following);
            (Some(opening), granted)
        }
        None => (None, None),
    };
    let info = review.info().ok().flatten();
    let synced = review.synced().ok().flatten();
    let history = Arc::new(history);
    let texts = read_texts(&review, &history, revision);
    let (view_state, ignored) = match crate::folders::load(&review) {
        Ok(view_state) => (view_state, None),
        Err(ignored) => (crate::folders::ViewState::default(), Some(ignored)),
    };
    Ok(Opened {
        review,
        view_state,
        ignored,
        found,
        trusted,
        granted,
        following,
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
