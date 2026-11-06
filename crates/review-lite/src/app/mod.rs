//! The review window, in lazygit's shape: the changed files on the left, the selected file's
//! diff on the right, and the whole file as a read-only buffer on request.
//!
//! The window keeps three kinds of state apart:
//!
//! - the [`state::Snapshot`] on screen, one revision of one history, and the file text and
//!   highlights it shows, cached by blob identity;
//! - the workspace: the selected file, the mode that owns the keyboard, the selected hunk, folds
//!   and scroll positions;
//! - the marks waiting for the engine, each with the exact units and the revision they were
//!   shown in.
//!
//! Everything slow (git, analysis, the event log, highlighting, language servers) runs on the
//! background executor; its results either fill caches keyed by content or carry the identity of
//! what they were computed for, so they can never land on the wrong view.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    App, Context, Entity, FocusHandle, Font, ScrollHandle, SharedString, Subscription, TextLayout,
    UniformListScrollHandle, Window, actions,
};
use gpui_component::input::InputState;
use gpui_component::scroll::ScrollbarHandle as _;
use parking_lot::Mutex;
use review_core::tree::{NodeKind, TreeNode};
use review_core::{History, MarkVerdict, ObjectId, Review, RevisionNumber, UnitId};

use crate::lsp::LanguageServer;
use crate::rows::{self, Folds, Layout};
use crate::syntax::SideSyntax;

mod buffer;
mod chrome;
mod diff;
mod find;
mod folders;
pub mod keymap;
mod markdown;
mod marks;
mod overview;
mod pairs;
mod panes;
mod picker;
mod present;
mod qa;
mod render;
mod scroll;
mod search;
mod select;
mod settings;
mod state;
mod text;
mod tree;

use keymap::Area;
use scroll::{Animation, PagedScroll};
use state::{MarkJob, Snapshot};
use text::{Pane, Selection};

actions!(
    review_lite,
    [
        TreeDown,
        TreeUp,
        TreeCollapse,
        TreeExpand,
        Activate,
        FocusDiff,
        CloseAllFolds,
        OpenAllFolds,
        NextHunk,
        PreviousHunk,
        FirstHunk,
        LastHunk,
        HalfPageDown,
        HalfPageUp,
        ToggleFold,
        NextFile,
        FocusTree,
        ToggleBuffer,
        ToggleApprove,
        ToggleFlag,
        ResetReview,
        Refresh,
        ToggleTrust,
        YankUnitId,
        CursorDown,
        CursorUp,
        NextWord,
        PreviousWord,
        BufferTop,
        BufferBottom,
        OpenFind,
        FindNext,
        FindPrevious,
        GoToDefinition,
        JumpBack,
        Dismiss,
        CloseFind,
        ToggleOverview,
        ToggleDiffStyle,
        ScreenNext,
        ScreenPrevious,
        ToggleSidebar,
        ToggleHelp,
        CloseOverlay,
        Copy,
        ClearFilter,
        FindFiles,
        SearchContent,
        PickerNext,
        PickerPrevious,
        PickerOpen,
        PickerScope,
        PickerBrowse,
        PickerEdit,
        PickerClose,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ToggleFullscreen,
        CloseWindow,
        Quit,
    ]
);

/// How far the scroll benchmark moves per frame: thirty rows, a fast fling.
const BENCH_ROWS_PER_FRAME: f32 = 30.0;

/// How many unchanged lines each expansion of a gap reveals.
const GAP_STEP: usize = 20;

/// How long go-to-definition waits for a language server that is still indexing.
const DEFINITION_DEADLINE: Duration = Duration::from_secs(120);

/// The smallest and largest code size zoom and the configuration allow, in logical pixels.
pub const ZOOM_RANGE: Range<f32> = 9.0..40.0;

/// Binds the review window's keys from the keymap.
pub fn bind_keys(bindings: &[keymap::Binding], cx: &mut App) {
    keymap::bind(bindings, cx);
}

/// What the app does once its window is up, besides waiting for the reviewer.
#[derive(Debug, Clone, Copy)]
pub struct Startup {
    /// When the process started, for the time to the first visible hunk.
    pub process_started: Instant,
    /// Scroll through every row once, print frame statistics, and quit.
    pub bench_scroll: Option<BenchMode>,
    /// Quit once the first hunk is visible, printing how long that took.
    pub exit_after_first_frame: bool,
    /// Read QA commands from standard input; see the `qa` module.
    pub qa: bool,
}

/// How the scroll benchmark moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchMode {
    /// Sets the offset directly every frame, like a fast fling.
    Fling,
    /// Pages down with the animated scrolling that keyboard jumps use.
    Animated,
}

/// The code font, which the whole window uses.
#[derive(Debug, Clone)]
pub struct Fonts {
    /// The code font, with ligatures off.
    pub code: Font,
    /// The code size in logical pixels.
    pub code_size: f32,
    /// The code size the window started with, for resetting zoom.
    pub default_code_size: f32,
    /// The line height as a multiple of the code size, which the shown look's density sets;
    /// only [`ReviewApp::transition`] changes it, so the lists keep their place.
    pub line_height: f32,
    /// A cell's width as a share of the font size, measured from the code font.
    ///
    /// The window's text is in the same monospace font, so columns sized in cells hold their
    /// text at any size.
    pub advance: f32,
}

impl Fonts {
    /// The size of the window's own text: labels, the tree and the bars.
    fn ui_size(&self) -> f32 {
        (self.code_size * 0.86).round()
    }

    /// The height of every row; the lists are uniform so they lay out only visible rows.
    fn row_height(&self) -> f32 {
        (self.code_size * self.line_height).round()
    }

    /// A cell's width in text `size` pixels tall.
    fn cell(&self, size: f32) -> f32 {
        size * self.advance
    }

    /// A cell of the window's own text.
    fn ui_cell(&self) -> f32 {
        self.cell(self.ui_size())
    }

    /// A cell of code.
    fn code_cell(&self) -> f32 {
        self.cell(self.code_size)
    }

    /// The height of a pane header, the find bar and the like, which hold one line of the
    /// window's text.
    fn bar(&self) -> f32 {
        render::bar_height(self.ui_size())
    }

    /// The height of each of the two bottom bars.
    fn strip(&self) -> f32 {
        render::strip_height(self.ui_size())
    }
}

/// View settings from the configuration file, and the look as the reviewer changes it.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Where the configuration file is, for remembering choices made in the app.
    pub config_path: Option<PathBuf>,
    /// Unchanged lines shown around each hunk.
    pub context: usize,
    /// Whether approved hunks fold, as the configuration sets it.
    pub approved: crate::config::Approved,
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
    /// The keymap, after the configuration's overrides.
    pub keymap: Vec<keymap::Binding>,
    /// What the configuration got wrong, which the status line names once the review opens.
    pub problems: Vec<String>,
}

/// The pane widths and whether the sidebar shows.
pub type PaneLayout = crate::config::LayoutConfig;

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
    /// The file finder or the content search; see [`ReviewApp::picker`].
    Picker,
    /// The settings list; see the `settings` module.
    Settings,
}

/// Whether the review is ready to show.
enum Phase {
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
struct Find {
    /// The mode it searches, [`Mode::Diff`] or [`Mode::Buffer`]; leaving that mode ends it.
    target: Mode,
    input: Entity<InputState>,
    query: String,
    /// Shared with each frame's rows rather than copied.
    matches: Arc<Vec<find::Match>>,
    current: Option<usize>,
    /// Where the search started, so the matches of a query being typed are found from there,
    /// as vim's incremental search does.
    origin: find::Spot,
    /// In the diff, the hunk the current match selected; once another hunk is selected, `n`
    /// and `N` search from there instead.
    hunk: Option<usize>,
    /// Whether the bar takes typing; after Enter, the matches stay for `n` and `N`.
    editing: bool,
    _subscription: Subscription,
}

/// The `/` filter of the file tree, which narrows the tree to the files whose path contains its
/// query and stays until cleared.
struct Filter {
    input: Entity<InputState>,
    query: String,
    /// Whether the bar takes typing; after Enter, the filter stays and the tree takes the keys.
    editing: bool,
    _subscription: Subscription,
}

/// A scroll benchmark in progress.
struct Bench {
    mode: BenchMode,
    started: Instant,
    last: Instant,
    intervals: Vec<Duration>,
    render_started: Option<Instant>,
    built: Option<Duration>,
    work: Vec<Duration>,
    rendered_rows: usize,
    /// Files scrolled through to their end, and their rows.
    files: usize,
    rows: usize,
}

/// Input-to-frame latency samples.
#[derive(Default)]
struct Latency {
    pending: Option<Instant>,
    samples: Vec<Duration>,
}

/// Running language servers by program and worktree.
type Servers = Arc<Mutex<HashMap<(&'static str, PathBuf), LanguageServer>>>;

/// The text layouts of the lines painted in the last frame, by pane, column and row, for
/// mapping the mouse to text.
type PaintedRows = Rc<RefCell<HashMap<(Pane, text::Column, usize), TextLayout>>>;

/// File text and highlights by blob, filled in the background; content never changes under an
/// object ID, so these never go stale.
#[derive(Default)]
struct Blobs {
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

/// How long the tree cursor rests before a previewed file's diff is built; a held key repeats
/// faster than this.
const PREVIEW_REST: Duration = Duration::from_millis(120);

/// The review window.
pub struct ReviewApp {
    phase: Phase,
    review: Option<Arc<Review>>,
    snapshot: Option<Snapshot>,
    blobs: Blobs,
    /// Blobs being read or highlighted right now, so none is requested twice at once; an entry
    /// goes when its work completes, so a blob shown again after pruning is requested again.
    requested: state::Outstanding,
    /// What the review is about.
    info: Option<review_core::ReviewInfo>,
    revisions: Vec<overview::RevisionSummary>,
    /// The file tree with pending marks applied.
    tree: Vec<TreeNode>,
    /// The folded folders, saved with the review by [`folders::Saver`].
    tree_collapsed: HashSet<String>,
    folders: folders::Saver,
    /// The visible tree nodes, from [`Self::update_visible_tree`].
    tree_visible: Arc<Vec<usize>>,
    tree_cursor: usize,
    tree_list: UniformListScrollHandle,
    /// The file whose diff shows, by display path.
    selected: Option<String>,
    mode: Mode,
    screen: Screen,
    overlay: Option<Overlay>,
    /// The selected file's diff.
    rows: Arc<Layout>,
    /// A file previewed while the tree cursor moves fast: only its header shows until the
    /// cursor rests, then its diff is built. Holds the preview's number, so a later move
    /// supersedes an earlier one.
    preview: Option<u64>,
    previews: u64,
    /// When the tree cursor last moved, to tell a held key from a single press.
    last_move: Option<Instant>,
    /// The presentation [`Self::build_selected`] projects the rows for; only
    /// [`Self::transition`] changes it.
    presented: rows::Presentation,
    /// The diff pane's width, which decides whether side by side fits.
    diff_width: f32,
    /// The side-by-side diff's text offset; see the `pairs` module.
    text_scroll: pairs::TextScroll,
    /// The settings list, while it is open.
    settings_panel: Option<settings::Panel>,
    /// The rows of each display hunk of [`Self::rows`].
    hunk_rows: Vec<Range<usize>>,
    /// The selected hunk, by index into the layout's hunks.
    hunk: usize,
    folds: Folds,
    places: HashMap<String, Place>,
    list: UniformListScrollHandle,
    list_bar: PagedScroll,
    list_animation: Option<Animation>,
    buffer: Option<buffer::Buffer>,
    /// The last buffer identity handed out.
    buffer_ids: u64,
    buffer_list: UniformListScrollHandle,
    buffer_bar: PagedScroll,
    buffer_animation: Option<Animation>,
    find: Option<Find>,
    filter: Option<Filter>,
    picker: Option<picker::Picker>,
    /// Pickers opened and searches run, one count for both, so every answer names its search.
    picker_runs: u64,
    marks: VecDeque<MarkJob>,
    marking: bool,
    /// Verdicts asked for but not yet recorded, shown at once.
    pending: HashMap<UnitId, MarkVerdict>,
    refreshing: bool,
    tree_focus: FocusHandle,
    diff_focus: FocusHandle,
    buffer_focus: FocusHandle,
    overlay_focus: FocusHandle,
    overview_scroll: ScrollHandle,
    /// The pane widths and whether the files show.
    layout: PaneLayout,
    /// Whether the sidebar divider is being dragged.
    resizing: bool,
    /// Named hit regions and window operations, for the title bar and QA mode.
    regions: Rc<chrome::Regions>,
    status: SharedString,
    /// When the status line last reported a failure, for tinting it.
    failed_at: Option<Instant>,
    looking_up: bool,
    /// Background reads and highlighting jobs still running.
    highlighting: usize,
    client_decorated: bool,
    /// Whether language servers may run on the reviewed code; see [`Review::trust`].
    trusted: bool,
    /// Whether trusting awaits a confirming second press.
    trust_pending: bool,
    selection: Option<Selection>,
    dragging: Option<Pane>,
    painted: PaintedRows,
    fonts: Fonts,
    settings: Settings,
    servers: Servers,
    bench: Option<Bench>,
    latency: Latency,
    startup: Startup,
    /// How long the first hunk took to show, from process start.
    first_hunk: Option<Duration>,
}

impl ReviewApp {
    /// Opens the review with `opener` in the background and shows it when it is ready.
    #[expect(
        clippy::too_many_lines,
        reason = "one struct literal that starts every part of the window's state"
    )]
    pub fn new(
        opener: Opener,
        fonts: Fonts,
        settings: Settings,
        startup: Startup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tree_focus = cx.focus_handle();
        window.focus(&tree_focus, cx);
        let list = UniformListScrollHandle::new();
        let buffer_list = UniformListScrollHandle::new();
        let mut fonts = fonts;
        let (presented, diff_width) = present::opening(
            &settings,
            &mut fonts,
            f32::from(window.viewport_size().width),
        );
        let app = Self {
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
            tree_list: UniformListScrollHandle::new(),
            selected: None,
            mode: Mode::Tree,
            screen: Screen::Split,
            overlay: None,
            rows: Arc::new(Layout::default()),
            preview: None,
            previews: 0,
            last_move: None,
            presented,
            diff_width,
            text_scroll: pairs::TextScroll::default(),
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
            list_bar: PagedScroll::new(&list),
            list,
            list_animation: None,
            buffer: None,
            buffer_ids: 0,
            buffer_bar: PagedScroll::new(&buffer_list),
            buffer_list,
            buffer_animation: None,
            find: None,
            filter: None,
            picker: None,
            picker_runs: 0,
            marks: VecDeque::new(),
            marking: false,
            pending: HashMap::new(),
            refreshing: false,
            tree_focus,
            diff_focus: cx.focus_handle(),
            buffer_focus: cx.focus_handle(),
            overlay_focus: cx.focus_handle(),
            overview_scroll: ScrollHandle::new(),
            layout: settings.layout,
            resizing: false,
            regions: Rc::new(chrome::Regions::default()),
            status: "Opening the review…".into(),
            failed_at: None,
            looking_up: false,
            highlighting: 0,
            client_decorated: false,
            trusted: false,
            trust_pending: false,
            selection: None,
            dragging: None,
            painted: PaintedRows::default(),
            fonts,
            settings,
            servers: Servers::default(),
            bench: None,
            latency: Latency::default(),
            startup,
            first_hunk: None,
        };
        let work = cx.background_executor().spawn(async move {
            let Loaded {
                review,
                history,
                found,
            } = opener()?;
            let trust = review.trust().map_err(|err| err.to_string())?;
            let info = review.info().ok().flatten();
            let revision = history.current().ok_or("the review has no revision yet")?;
            let history = Arc::new(history);
            let texts = read_texts(&review, &history, revision);
            let collapsed = folders::load(&review);
            Ok::<_, String>(Opened {
                review,
                collapsed,
                found,
                trusted: trust.trusted,
                info,
                snapshot: Snapshot { history, revision },
                texts,
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            this.update_in(cx, |this, window, cx| this.opened(result, window, cx))
                .ok();
        })
        .detach();
        if startup.qa {
            qa::start(window, cx);
        }
        app
    }

    /// Shows the opened review, or why it could not be opened.
    fn opened(
        &mut self,
        opened: Result<Opened, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match opened {
            Ok(opened) => {
                self.info = opened.info;
                self.trusted = opened.trusted;
                self.review = Some(Arc::new(opened.review));
                self.blobs.texts.extend(opened.texts);
                self.revisions = overview::summarize(&opened.snapshot.history);
                self.snapshot = Some(opened.snapshot);
                window.set_window_title(&self.title());
                self.phase = Phase::Ready;
                self.refresh_tree();
                self.restore_folders(opened.collapsed, cx);
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
                self.request_syntax(cx);
            }
            Err(err) => {
                eprintln!("review-lite: {err}");
                self.status = "The review could not be opened.".into();
                self.phase = Phase::Failed(err);
            }
        }
        let process_started = self.startup.process_started;
        cx.on_next_frame(window, move |this, window, cx| {
            this.first_hunk = Some(process_started.elapsed());
            if let Some(mode) = this.startup.bench_scroll {
                this.start_bench(mode, window, cx);
            } else if this.startup.exit_after_first_frame {
                println!(
                    "first hunk visible after {:.1?}: {} files, {} rows",
                    process_started.elapsed(),
                    this.tree.len(),
                    this.rows.rows.len()
                );
                cx.quit();
            }
        });
        cx.notify();
    }

    /// The window title: the pull request or range under review.
    fn title(&self) -> String {
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

    /// One line on where the review stands.
    fn summary(&self) -> String {
        let (reviewed, total) = self.totals();
        let revision = self.snapshot.as_ref().map_or_else(String::new, |snapshot| {
            format!("Revision {}", snapshot.revision)
        });
        format!(
            "{revision} · {reviewed} of {} reviewed",
            plural(total, "hunk")
        )
    }

    /// Files whose every unit is reviewed, and all files, pending marks included.
    fn file_totals(&self) -> (usize, usize) {
        self.tree
            .iter()
            .filter(|node| node.kind == NodeKind::File)
            .fold((0, 0), |(done, total), node| {
                let counts = &node.counts;
                let reviewed = counts.reviewed + counts.accepted + counts.flagged;
                (done + usize::from(reviewed >= counts.units), total + 1)
            })
    }

    /// Reviewed and total units over every file, pending marks included.
    fn totals(&self) -> (usize, usize) {
        self.tree
            .iter()
            .filter(|node| node.depth == 0)
            .fold((0, 0), |(reviewed, total), node| {
                let counts = &node.counts;
                (
                    reviewed + counts.reviewed + counts.accepted + counts.flagged,
                    total + counts.units,
                )
            })
    }

    fn set_status(&mut self, status: impl Into<SharedString>) {
        self.status = status.into();
        self.failed_at = None;
    }

    fn fail(&mut self, status: impl Into<SharedString>) {
        self.status = status.into();
        self.failed_at = Some(Instant::now());
    }

    /// The shown revision's view.
    fn view(&self) -> Option<review_core::RevisionView<'_>> {
        self.snapshot.as_ref()?.view()
    }

    /// The area whose keys apply now.
    fn area(&self) -> Area {
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
    fn typing(&self) -> bool {
        match self.mode {
            Mode::Tree => self.filter.as_ref().is_some_and(|filter| filter.editing),
            mode => self
                .find
                .as_ref()
                .is_some_and(|find| find.editing && find.target == mode),
        }
    }

    /// Gives the keyboard to the current mode or overlay.
    fn focus_mode(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay == Some(Overlay::Settings) && self.focus_settings(window, cx) {
            return;
        }
        if self.overlay == Some(Overlay::Picker)
            && let Some(picker) = self.picker.as_ref().filter(|picker| picker.editing)
        {
            picker.input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let handle = match (self.overlay, self.mode) {
            (Some(_), _) => &self.overlay_focus,
            (None, Mode::Tree) => &self.tree_focus,
            (None, Mode::Diff) => &self.diff_focus,
            (None, Mode::Buffer) => &self.buffer_focus,
        };
        window.focus(handle, cx);
    }

    /// Switches to `mode` and gives it the keyboard.
    fn enter_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        // A mode works on the diff of the selected file, never on a preview's header alone.
        self.finish_preview();
        // A search belongs to the mode it searches.
        if self.find.as_ref().is_some_and(|find| find.target != mode) {
            self.find = None;
        }
        self.mode = mode;
        self.selection = None;
        self.focus_mode(window, cx);
        cx.notify();
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
    fn hunk_units(&self) -> Vec<UnitId> {
        self.rows.hunks.get(self.hunk).cloned().unwrap_or_default()
    }

    /// The row the diff's keyboard target starts at: the selected hunk's header.
    fn hunk_start(&self) -> Option<usize> {
        self.hunk_rows.get(self.hunk).map(|rows| rows.start)
    }

    fn toggle_help(&mut self, _: &ToggleHelp, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::Help);
        self.focus_mode(window, cx);
        cx.notify();
    }

    fn toggle_overview(&mut self, _: &ToggleOverview, window: &mut Window, cx: &mut Context<Self>) {
        self.overlay = Some(Overlay::Overview);
        self.focus_mode(window, cx);
        cx.notify();
    }

    fn close_overlay(&mut self, _: &CloseOverlay, window: &mut Window, cx: &mut Context<Self>) {
        if self.overlay == Some(Overlay::Settings) {
            self.close_settings(window, cx);
            return;
        }
        self.overlay = None;
        self.focus_mode(window, cx);
        cx.notify();
    }

    /// `|`: the next diff style, remembered.
    fn toggle_diff_style(&mut self, _: &ToggleDiffStyle, _: &mut Window, cx: &mut Context<Self>) {
        self.next_diff_style();
        cx.notify();
    }

    /// `+`: the diff takes the whole width.
    fn screen_next(&mut self, _: &ScreenNext, _: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Full;
        cx.notify();
    }

    /// `_`: back to files and diff side by side.
    fn screen_previous(&mut self, _: &ScreenPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.screen = Screen::Split;
        self.layout.sidebar_visible = true;
        self.focus_mode(window, cx);
        cx.notify();
    }

    /// `Ctrl+B`: shows or hides the files, and remembers it.
    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        self.layout.sidebar_visible = !self.layout.sidebar_visible;
        self.screen = if self.layout.sidebar_visible {
            Screen::Split
        } else {
            Screen::Full
        };
        if !self.layout.sidebar_visible && self.mode == Mode::Tree {
            self.enter_mode(Mode::Diff, window, cx);
        }
        self.remember_layout();
        cx.notify();
    }

    /// Whether the files show beside the diff.
    fn sidebar_shown(&self) -> bool {
        self.screen == Screen::Split && self.layout.sidebar_visible
    }

    /// The sidebar's width as shown, zero when hidden.
    fn shown_sidebar_width(&self, window: &Window) -> f32 {
        if self.sidebar_shown() {
            panes::sidebar_width(
                self.layout.sidebar,
                f32::from(window.viewport_size().width),
                self.fonts.ui_cell(),
            )
        } else {
            0.0
        }
    }

    /// Writes the pane width to the configuration file, for the next launch.
    fn remember_layout(&mut self) {
        let Some(path) = self.settings.config_path.clone() else {
            return;
        };
        let layout = self.layout;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a pane width in logical pixels, clamped far below i64's range"
        )]
        let sidebar = layout.sidebar.round() as i64;
        let values = [
            crate::config::Remembered {
                table: "layout",
                key: "sidebar",
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

    fn zoom(&mut self, size: Option<f32>, cx: &mut Context<Self>) {
        self.set_code_size(size.unwrap_or(self.fonts.default_code_size), cx);
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(Some(self.fonts.code_size + 1.0), cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(Some(self.fonts.code_size - 1.0), cx);
    }

    fn zoom_reset(&mut self, _: &ZoomReset, _: &mut Window, cx: &mut Context<Self>) {
        self.zoom(None, cx);
    }

    /// Reads, in the background, the head text of the shown revision's files that are not
    /// cached yet, then highlights both versions of every file.
    fn request_syntax(&mut self, cx: &mut Context<Self>) {
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
        let work = cx.background_executor().spawn(async move {
            let mut texts = HashMap::new();
            let mut syntax = HashMap::new();
            let Ok(mut reader) = review.repo().blob_reader() else {
                return (texts, syntax);
            };
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
            (texts, syntax)
        });
        cx.spawn(async move |this, cx| {
            let (texts, syntax) = work.await;
            this.update(cx, |this, cx| {
                this.highlighting = this.highlighting.saturating_sub(1);
                this.requested.finish(&asked);
                this.blobs.texts.extend(texts);
                this.blobs.syntax.extend(syntax);
                // A late answer for a revision no longer shown is not kept.
                if let Some(view) = this.snapshot.as_ref().and_then(state::Snapshot::view) {
                    this.blobs.retain_for(&view);
                }
                this.rebuild();
                this.refresh_buffer_syntax();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_bench(&mut self, mode: BenchMode, window: &mut Window, cx: &mut Context<Self>) {
        // The bench walks every file of the review, from the first.
        if let Some(first) = file_paths(&self.tree)
            .first()
            .map(|path| (*path).to_owned())
        {
            self.show_file(&first);
        }
        let now = Instant::now();
        self.bench = Some(Bench {
            mode,
            started: now,
            last: now,
            intervals: Vec::new(),
            render_started: None,
            built: None,
            work: Vec::new(),
            rendered_rows: 0,
            files: 0,
            rows: 0,
        });
        cx.notify();
        cx.on_next_frame(window, ReviewApp::bench_frame);
    }

    /// Records the interval since the last frame and scrolls further, or reports and quits at
    /// the end of the list.
    fn bench_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row_height = self.fonts.row_height();
        let Some(bench) = &mut self.bench else {
            return;
        };
        let now = Instant::now();
        bench.intervals.push(now - bench.last);
        bench.last = now;
        if let Some(built) = bench.built.take() {
            bench.work.push(built);
        }
        let max =
            f32::from(self.list.content_size().height - self.list.viewport_bounds().size.height);
        let current = scroll::offset_of(&self.list);
        if bench.intervals.len() > 1 && current >= max - 0.5 {
            // The end of one file: on to the next, so the bench covers the whole review.
            bench.files += 1;
            bench.rows += self.rows.rows.len();
            let paths = file_paths(&self.tree);
            let next = self
                .selected
                .as_deref()
                .and_then(|selected| paths.iter().position(|path| *path == selected))
                .and_then(|position| paths.get(position + 1))
                .map(|path| (*path).to_owned());
            if let Some(path) = next {
                self.list_animation = None;
                self.show_file(&path);
                scroll::set_offset_of(&self.list, 0.0);
                cx.notify();
                cx.on_next_frame(window, ReviewApp::bench_frame);
            } else {
                println!("{}", bench_report(bench));
                cx.quit();
            }
            return;
        }
        match bench.mode {
            BenchMode::Fling => {
                scroll::set_offset_of(
                    &self.list,
                    (current + BENCH_ROWS_PER_FRAME * row_height).min(max),
                );
            }
            BenchMode::Animated => {
                if self.list_animation.is_none() {
                    let page =
                        f32::from(self.list.viewport_bounds().size.height) - 2.0 * row_height;
                    self.scroll_list_to(current + page);
                }
            }
        }
        cx.notify();
        cx.on_next_frame(window, ReviewApp::bench_frame);
    }

    /// Records the time from an input event to the end of the frame that shows its effect.
    fn note_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.latency.pending.is_some() {
            cx.on_next_frame(window, |this, _, _| {
                if let Some(started) = this.latency.pending.take() {
                    this.latency.samples.push(started.elapsed());
                }
            });
        }
    }
}

/// What opening a review produced.
struct Opened {
    review: Review,
    /// The folded folders saved with the review.
    collapsed: Vec<String>,
    found: Option<String>,
    trusted: bool,
    info: Option<review_core::ReviewInfo>,
    snapshot: Snapshot,
    texts: HashMap<ObjectId, Arc<rows::FullText>>,
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
fn file_paths(tree: &[TreeNode]) -> Vec<&str> {
    tree.iter()
        .filter(|node| node.kind == NodeKind::File)
        .map(|node| node.path.as_str())
        .collect()
}

/// `count` followed by `noun`, pluralized.
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Summarizes the scroll benchmark's frame intervals and per-frame work.
fn bench_report(bench: &Bench) -> String {
    let intervals: Vec<Duration> = bench.intervals.iter().skip(1).copied().collect();
    format!(
        "scrolled {} rows of {} files ({:?}) in {} frames over {:.1?} ({} rows rendered)\n\
         frame interval: {}\n\
         render through row building: {}",
        bench.rows,
        bench.files,
        bench.mode,
        intervals.len(),
        bench.last - bench.started,
        bench.rendered_rows,
        distribution(intervals),
        distribution(bench.work.clone()),
    )
}

/// Describes the percentiles of `values` and how many exceed one and two 60 Hz frames.
fn distribution(mut values: Vec<Duration>) -> String {
    values.sort_unstable();
    let percentile = |fraction: f64| -> Duration {
        let last = values.len().saturating_sub(1);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "an index into a list of frame durations"
        )]
        let index = ((last as f64) * fraction).round() as usize;
        values.get(index).copied().unwrap_or_default()
    };
    let over = |limit: Duration| values.iter().filter(|&&value| value > limit).count();
    format!(
        "p50 {:.2?}, p99 {:.2?}, max {:.2?}; over 8.3 ms: {}, over 16.7 ms: {}",
        percentile(0.5),
        percentile(0.99),
        values.last().copied().unwrap_or_default(),
        over(Duration::from_micros(8_300)),
        over(Duration::from_micros(16_700)),
    )
}

#[cfg(test)]
mod tests {
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
