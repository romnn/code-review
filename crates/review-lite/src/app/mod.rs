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
pub mod keymap;
mod markdown;
mod marks;
mod overview;
mod panes;
mod qa;
mod render;
mod scroll;
mod select;
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
        CycleTheme,
        ScreenNext,
        ScreenPrevious,
        ToggleSidebar,
        ToggleHelp,
        CloseOverlay,
        Copy,
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

/// The smallest and largest code size zoom allows, in logical pixels.
const ZOOM_RANGE: Range<f32> = 9.0..40.0;

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
}

impl Fonts {
    /// The size of the window's own text: labels, the tree and the bars.
    fn ui_size(&self) -> f32 {
        (self.code_size * 0.86).round()
    }

    /// The height of every row; the lists are uniform so they lay out only visible rows.
    fn row_height(&self) -> f32 {
        (self.code_size * 1.5).round()
    }
}

/// View settings from the configuration file.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Where the configuration file is, for remembering choices made in the app.
    pub config_path: Option<PathBuf>,
    /// Unchanged lines shown around each hunk.
    pub context: usize,
    /// How changed lines are colored.
    pub diff_style: crate::config::DiffStyle,
    /// The removed-line background, as `0xRRGGBB`, when the configuration overrides the
    /// theme's.
    pub removed: Option<u32>,
    /// The added-line background, likewise.
    pub added: Option<u32>,
    /// The theme in use.
    pub theme: Arc<crate::theme::Theme>,
    /// Every theme `T` cycles through.
    pub themes: Vec<crate::theme::Theme>,
    /// Whether changed tokens within a line are emphasized.
    pub token_emphasis: bool,
    /// The pane widths remembered from the last session.
    pub layout: PaneLayout,
    /// The keymap, after the configuration's overrides.
    pub keymap: Vec<keymap::Binding>,
}

/// The pane widths and whether the sidebar shows.
pub type PaneLayout = crate::config::LayoutConfig;

/// Opens the review and loads its history, off the UI thread.
pub type Opener = Box<dyn FnOnce() -> Result<(Review, History), String> + Send>;

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

/// Where the reviewer was in a file, kept while other files show.
#[derive(Debug, Clone, Copy, Default)]
struct Place {
    hunk: usize,
    offset: f32,
}

/// The find bar of the buffer.
struct Find {
    input: Entity<InputState>,
    query: String,
    matches: Vec<find::Match>,
    current: Option<usize>,
    /// Whether the bar takes typing; after Enter, the matches stay for `n` and `N`.
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
}

/// Input-to-frame latency samples.
#[derive(Default)]
struct Latency {
    pending: Option<Instant>,
    samples: Vec<Duration>,
}

/// Running language servers by program and worktree.
type Servers = Arc<Mutex<HashMap<(&'static str, PathBuf), LanguageServer>>>;

/// The text layouts of the rows painted in the last frame, for mapping the mouse to text.
type PaintedRows = Rc<RefCell<HashMap<(Pane, usize), TextLayout>>>;

/// File text and highlights by blob, filled in the background; content never changes under an
/// object ID, so these never go stale.
#[derive(Default)]
struct Blobs {
    texts: HashMap<ObjectId, Arc<rows::FullText>>,
    syntax: HashMap<ObjectId, Arc<SideSyntax>>,
}

/// The review window.
pub struct ReviewApp {
    phase: Phase,
    review: Option<Arc<Review>>,
    snapshot: Option<Snapshot>,
    blobs: Blobs,
    /// Blobs being read or highlighted, so each is requested once.
    requested: HashSet<ObjectId>,
    /// What the review is about.
    info: Option<review_core::ReviewInfo>,
    revisions: Vec<overview::RevisionSummary>,
    /// The file tree with pending marks applied.
    tree: Vec<TreeNode>,
    tree_collapsed: HashSet<String>,
    tree_cursor: usize,
    tree_list: UniformListScrollHandle,
    /// The file whose diff shows, by display path.
    selected: Option<String>,
    mode: Mode,
    screen: Screen,
    overlay: Option<Overlay>,
    /// The selected file's diff.
    rows: Layout,
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
    buffer_list: UniformListScrollHandle,
    buffer_bar: PagedScroll,
    buffer_animation: Option<Animation>,
    find: Option<Find>,
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
    highlighting: bool,
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
        let app = Self {
            phase: Phase::Loading(Instant::now()),
            review: None,
            snapshot: None,
            blobs: Blobs::default(),
            requested: HashSet::new(),
            info: None,
            revisions: Vec::new(),
            tree: Vec::new(),
            tree_collapsed: HashSet::new(),
            tree_cursor: 0,
            tree_list: UniformListScrollHandle::new(),
            selected: None,
            mode: Mode::Tree,
            screen: Screen::Split,
            overlay: None,
            rows: Layout::default(),
            hunk_rows: Vec::new(),
            hunk: 0,
            folds: Folds {
                context: settings.context,
                single_file: true,
                ..Folds::default()
            },
            places: HashMap::new(),
            list_bar: PagedScroll::new(&list),
            list,
            list_animation: None,
            buffer: None,
            buffer_bar: PagedScroll::new(&buffer_list),
            buffer_list,
            buffer_animation: None,
            find: None,
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
            highlighting: false,
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
            let (review, history) = opener()?;
            let trust = review.trust().map_err(|err| err.to_string())?;
            let info = review.info().ok().flatten();
            let revision = history.current().ok_or("the review has no revision yet")?;
            let history = Arc::new(history);
            let texts = read_texts(&review, &history, revision);
            Ok::<_, String>(Opened {
                review,
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
                self.select_first_file();
                self.status = self.summary().into();
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
        let editing = self.find.as_ref().is_some_and(|find| find.editing);
        match (self.overlay, editing, self.mode) {
            (Some(_), ..) => Area::Overlay,
            (None, true, Mode::Buffer) => Area::Find,
            (None, _, Mode::Tree) => Area::Tree,
            (None, _, Mode::Diff) => Area::Diff,
            (None, _, Mode::Buffer) => Area::Buffer,
        }
    }

    /// Gives the keyboard to the current mode or overlay.
    fn focus_mode(&self, window: &mut Window, cx: &mut Context<Self>) {
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
        self.tree = review_core::tree::file_tree(&files);
    }

    /// Lays out the selected file's diff from the current state, keeping the selected hunk.
    fn rebuild(&mut self) {
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let Some(view) = snapshot.view() else {
            return;
        };
        let Some(path) = self.selected.clone() else {
            self.rows = Layout::default();
            self.hunk_rows.clear();
            return;
        };
        let units = view.units_under(std::slice::from_ref(&path));
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
        self.rows = rows::build(&view, &units, &self.folds);
        self.hunk_rows = hunk_rows(&self.rows);
        self.hunk = self.hunk.min(self.hunk_rows.len().saturating_sub(1));
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
        self.overlay = None;
        self.focus_mode(window, cx);
        cx.notify();
    }

    /// `|`: switches between syntax and plain diff colors and remembers the choice.
    fn toggle_diff_style(&mut self, _: &ToggleDiffStyle, _: &mut Window, cx: &mut Context<Self>) {
        let style = self.settings.diff_style.toggled();
        self.settings.diff_style = style;
        let saved = self.settings.config_path.as_deref().map(|path| {
            crate::config::remember(path, &[("diff", "style", format!("{:?}", style.name()))])
        });
        match saved {
            Some(Err(err)) => self.fail(format!(
                "Showing {} diffs; could not remember it: {err}",
                style.name()
            )),
            _ => self.set_status(format!("Showing {} diffs.", style.name())),
        }
        cx.notify();
    }

    /// `T`: the next theme, remembered in the configuration.
    fn cycle_theme(&mut self, _: &CycleTheme, _: &mut Window, cx: &mut Context<Self>) {
        let themes = &self.settings.themes;
        let next = themes
            .iter()
            .position(|theme| theme.name == self.settings.theme.name)
            .map_or(0, |position| (position + 1) % themes.len().max(1));
        let Some(theme) = themes.get(next).cloned() else {
            return;
        };
        let title = theme.title.clone();
        let name = theme.name.clone();
        self.settings.theme = Arc::new(theme);
        let saved =
            self.settings.config_path.as_deref().map(|path| {
                crate::config::remember(path, &[("theme", "name", format!("{name:?}"))])
            });
        match saved {
            Some(Err(err)) => self.fail(format!("Theme {title}; could not remember it: {err}")),
            _ => self.set_status(format!("Theme {title}.")),
        }
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
            panes::sidebar_width(self.layout.sidebar, f32::from(window.viewport_size().width))
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
        let values = [
            ("layout", "sidebar", format!("{}", layout.sidebar.round())),
            (
                "layout",
                "sidebar_visible",
                layout.sidebar_visible.to_string(),
            ),
        ];
        if let Err(err) = crate::config::remember(&path, &values) {
            self.fail(format!("Could not remember the pane width: {err}"));
        }
    }

    fn zoom(&mut self, size: Option<f32>, cx: &mut Context<Self>) {
        let size = size.unwrap_or(self.fonts.default_code_size);
        self.fonts.code_size = size.clamp(ZOOM_RANGE.start, ZOOM_RANGE.end);
        self.set_status(format!("Code size {:.1} px.", self.fonts.code_size));
        cx.notify();
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
            for side in [file.new.as_ref(), file.old.as_ref()].into_iter().flatten() {
                if !self.blobs.syntax.contains_key(&side.oid)
                    && self.requested.insert(side.oid.clone())
                {
                    wanted.push((side.path.clone(), side.oid.clone()));
                }
            }
        }
        let texts_wanted: Vec<ObjectId> = analysis
            .files
            .iter()
            .filter_map(|file| file.new.as_ref())
            .filter(|side| !self.blobs.texts.contains_key(&side.oid))
            .map(|side| side.oid.clone())
            .collect();
        if wanted.is_empty() && texts_wanted.is_empty() {
            return;
        }
        self.highlighting = true;
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
                if review_core::syntax::Language::from_path(&path).is_none() {
                    continue;
                }
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
                this.highlighting = false;
                this.blobs.texts.extend(texts);
                this.blobs.syntax.extend(syntax);
                this.rebuild();
                this.refresh_buffer_syntax();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_bench(&mut self, mode: BenchMode, window: &mut Window, cx: &mut Context<Self>) {
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
            println!("{}", bench_report(bench, self.rows.rows.len()));
            cx.quit();
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
    trusted: bool,
    info: Option<review_core::ReviewInfo>,
    snapshot: Snapshot,
    texts: HashMap<ObjectId, Arc<rows::FullText>>,
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
    for side in analysis.files.iter().filter_map(|file| file.new.as_ref()) {
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
fn bench_report(bench: &Bench, rows: usize) -> String {
    let intervals: Vec<Duration> = bench.intervals.iter().skip(1).copied().collect();
    format!(
        "scrolled {rows} rows ({:?}) in {} frames over {:.1?} ({} rows rendered)\n\
         frame interval: {}\n\
         render through row building: {}",
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
