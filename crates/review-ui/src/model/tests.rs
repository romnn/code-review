//! The model on a fake synchronous host, for the races between input and background work that
//! scripted QA cannot pin: jobs run inline only when a test runs them, in the order it picks,
//! and timers fire only by hand.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indoc::{formatdoc, indoc};
use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};
use review_core::tree::NodeKind;
use review_core::{MarkVerdict, Review, RevisionNumber};

use super::buffer::{Definition, Lookup};
use super::effects::{Completion, Job, Outcome, Subject};
use super::picker::{Found, PreviewText, Unchanged};
use super::*;
use crate::frame::{Chrome, Geometry, Heights, List, Scale, Sizes, Wheel};
use crate::look::{Appearance, Look};
use crate::rows::Presentation;
use crate::scroll;
use crate::text::{Column, Pane, TextPos};

/// The code cells a line has room for in the fixed window, across the diff and in the buffer.
const CODE_ROOM: f64 = 40.0;

/// The sizes of the fixed window, in cells whatever the code size, with nothing beside a row's
/// code: no frame bar, gutter or sign, and no room after a buffer's line numbers.
fn cells() -> Sizes {
    Sizes {
        row_height: 1.0,
        code_cell: 1.0,
        text_cell: 1.0,
        frame: 0.0,
        divider: 0.0,
        gutter: 0.0,
        sign: 0.0,
        marker: 0.0,
        number_room: 0.0,
        half_slack: 0.0,
        note_spacing: 0.0,
        page_context: 2.0,
        chrome: Chrome::default(),
        panel_row_height: 1.0,
    }
}

/// A window whose lists each show forty rows (the picker twenty, the settings list thirty),
/// whose lines have room for forty cells of code, and whose diff pane leaves each half of a
/// side-by-side diff a hundred.
fn fixed_window() -> Geometry {
    Geometry {
        laid_out: settings().code_size,
        diff_width: 200.0,
        diff_list: 40.0,
        diff_half: 100.0,
        // Room for forty cells after the four digits of a short file's line numbers.
        buffer_list: 44.0,
        heights: Heights {
            diff: Some(40.0),
            buffer: Some(40.0),
            tree: Some(40.0),
            picker: Some(20.0),
            settings: Some(30.0),
        },
    }
}

/// A host that runs nothing by itself: the effects of every update wait here until the test
/// runs a job or fires a timer.
struct Host {
    driver: Driver,
    /// The review the window opened, as another window would hold it.
    review: Review,
    jobs: VecDeque<Job>,
    timers: Vec<Completion>,
    /// The minute clock's ticks, and a live review's periodic captures, which keep themselves
    /// going, so only a test fires them.
    clocks: Vec<Completion>,
    /// Jobs that wait for something outside the app, which only a test runs.
    waits: Vec<Job>,
    /// What the model copied to the clipboard.
    copied: Vec<String>,
    /// What the model put in the primary selection.
    primary: Vec<String>,
    /// How the model asked the front end to scroll its sheets.
    sheet_steps: Vec<SheetStep>,
    /// How many times the model asked the front end to quit.
    quits: usize,
    /// The diagnostics the model logged.
    logged: Vec<String>,
    /// The window titles the model asked for, in order.
    titles: Vec<String>,
}

impl Host {
    /// The window over `review`, opened and settled: the review read, its texts and highlights
    /// in, and every save done.
    fn open(review: Review) -> Self {
        Self::open_with(review, settings())
    }

    /// The window over `review` with `settings`, opened and settled.
    fn open_with(review: Review, settings: Settings) -> Self {
        Self::open_loaded(loaded(review), settings)
    }

    /// The window over the review `loaded` opens, with `settings`, opened and settled.
    fn open_loaded(loaded: Loaded, settings: Settings) -> Self {
        let mut host = Self::opening(loaded, settings);
        host.settle();
        assert!(
            matches!(host.model().phase, Phase::Ready),
            "the review opens"
        );
        host
    }

    /// The window over the review `loaded` opens, with `settings`, whose opening job waits to
    /// run.
    fn opening(loaded: Loaded, settings: Settings) -> Self {
        let mut host = Self {
            driver: Driver::new(Model::new(settings, Scale::fixed(cells()), fixed_window())),
            review: loaded.review.clone(),
            jobs: VecDeque::new(),
            timers: Vec::new(),
            clocks: Vec::new(),
            waits: Vec::new(),
            copied: Vec::new(),
            primary: Vec::new(),
            sheet_steps: Vec::new(),
            quits: 0,
            logged: Vec::new(),
            titles: Vec::new(),
        };
        let opener: Opener = Box::new(move || Ok(loaded));
        host.update(|model| model.open(opener));
        host
    }

    fn model(&self) -> &Model {
        self.driver.model()
    }

    /// Changes the model as a front end does, keeping the effects for the test to run.
    fn update<R>(&mut self, change: impl FnOnce(&mut Model) -> R) -> R {
        self.try_update(change)
            .expect("the model takes no change once the app closes")
    }

    /// Changes the model as a front end does, unless the app closes, and keeps the effects for
    /// the test to run; returns what `change` returned, if it ran.
    fn try_update<R>(&mut self, change: impl FnOnce(&mut Model) -> R) -> Option<R> {
        let Updated { value, effects } = self.driver.update(change);
        self.keep(effects);
        value
    }

    /// Keeps `effects` for the test to run.
    fn keep(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Spawn(job) if job.waits => self.waits.push(job),
                Effect::Spawn(job) => self.jobs.push_back(job),
                // The minute clock and the periodic capture keep themselves going, so they
                // wait apart from the timers that settle.
                Effect::After(_, completion)
                    if matches!(
                        *completion.outcome,
                        Outcome::Tick(
                            super::sync::Tick::Minute
                                | super::sync::Tick::Reconcile
                                | super::sync::Tick::PullRequest
                                | super::sync::Tick::Guard
                        )
                    ) =>
                {
                    self.clocks.push(completion);
                }
                Effect::After(_, completion) => self.timers.push(completion),
                Effect::Copy(text) => self.copied.push(text),
                Effect::CopyPrimary(text) => self.primary.push(text),
                Effect::ScrollSheet(step) => self.sheet_steps.push(step),
                Effect::Quit => self.quits += 1,
                Effect::Title(title) => self.titles.push(title),
                Effect::Fullscreen => {}
                Effect::Log(message) => self.logged.push(message),
            }
        }
    }

    fn dispatch(&mut self, action: Action) {
        self.try_update(|model| model.dispatch(action));
    }

    /// Delivers a key bound to `action` as a front end does, `held` saying whether it repeats.
    fn key(&mut self, action: Action, held: bool) {
        self.try_update(|model| model.key(action, held));
    }

    /// Runs the oldest waiting job and returns its completion, not yet applied.
    fn run_job(&mut self) -> Completion {
        let job = self.jobs.pop_front().expect("a job waits");
        job.run()
    }

    /// Runs the newest waiting job and returns its completion, not yet applied.
    fn run_newest_job(&mut self) -> Completion {
        let job = self.jobs.pop_back().expect("a job waits");
        job.run()
    }

    /// Hands a job's completion to the model on the UI thread, as the front end does when the
    /// job ends.
    fn complete(&mut self, completion: Completion) {
        let effects = self.driver.complete(completion);
        self.keep(effects);
    }

    /// Fires the `index`th waiting timer.
    fn fire(&mut self, index: usize) {
        let completion = self.timers.remove(index);
        self.complete(completion);
    }

    /// Runs every job and fires every timer, and what they start in turn, drawing a frame,
    /// which shows the opened review or a capture that waits for one, whenever none is left,
    /// until none waits.
    fn settle(&mut self) {
        loop {
            if let Some(job) = self.jobs.pop_front() {
                let completion = job.run();
                self.complete(completion);
            } else if !self.timers.is_empty() {
                self.fire(0);
            } else if self.model().publishes() {
                self.draw();
            } else {
                break;
            }
        }
    }

    /// Does what drawing a frame does to the model: shows the opened review or the capture that
    /// waits for it.
    fn draw(&mut self) {
        self.try_update(Model::publish);
    }
}

/// `review` with its history, as an opener hands it over.
fn loaded(review: Review) -> Loaded {
    let history = review.history().unwrap();
    Loaded {
        review,
        history: Some(history),
        found: None,
    }
}

/// The configuration's defaults, with no configuration file.
fn settings() -> Settings {
    let config = crate::config::Config::default();
    let (themes, _) = crate::theme::all(None);
    let theme = themes
        .iter()
        .find(|theme| theme.name == crate::theme::DEFAULT)
        .cloned()
        .unwrap();
    let styles = crate::look::styles(&BTreeMap::new());
    let style = crate::look::find_style(&styles, crate::look::DEFAULT_STYLE).unwrap();
    let (keymap, _) = keymap::keymap(&BTreeMap::new());
    Settings {
        config_path: None,
        code_size: 17.0,
        context: config.context,
        shorten_chains: config.shorten_chains,
        mouse: crate::config::Mouse::default(),
        approved: config.approved,
        policy: review_core::ReviewPolicy {
            auto_approve_formatting: config.auto_approve_formatting,
        },
        language_servers: true,
        live: config.live,
        removed: None,
        added: None,
        appearance: Appearance {
            applied: Look {
                theme: Arc::new(theme),
                style,
                density: config.density,
                layout: config.diff_layout,
                // These scenarios were written for hunks apart; the whole file's have their own.
                view: crate::config::DiffView::Hunks,
                hunk_position: config.hunk_position,
                cross_files: config.cross_files,
                soft_unselected: config.soft_unselected,
                token_emphasis: config.token_emphasis,
                wrap: config.wrap,
            },
            preview: None,
        },
        themes,
        styles,
        layout: PaneLayout {
            sidebar: config.layout.sidebar,
            comparison: config.layout.comparison,
            sidebar_visible: config.layout.sidebar_visible,
        },
        keymap,
        front_end: FrontEnd::Window,
        problems: Vec::new(),
    }
}

/// The defaults, with every unit needing a mark: for scenarios built on the demo's
/// formatting-only hunks still needing review.
fn marks_only() -> Settings {
    Settings {
        policy: review_core::ReviewPolicy::MARKS_ONLY,
        ..settings()
    }
}

/// The demo review, fresh, under `dir`.
fn demo(dir: &Path) -> Review {
    prepare(&DemoPaths::under(dir), &fixture_dir(), DemoRequest::Fresh).unwrap()
}

/// The positions in the visible tree of three files in a row.
fn three_files(model: &Model) -> [usize; 3] {
    let visible = model.visible_tree();
    let is_file = |position: usize| {
        visible
            .get(position)
            .and_then(|&index| model.tree.get(index))
            .is_some_and(|node| node.kind == NodeKind::File)
    };
    let first = (0..visible.len())
        .find(|&position| (position..position + 3).all(is_file))
        .expect("the demo's tree has three files in a row");
    [first, first + 1, first + 2]
}

/// The path of the node at `position` of the visible tree.
fn path_at(model: &Model, position: usize) -> String {
    let index = model.visible_tree()[position];
    model.tree[index].path.clone()
}

/// A held key shows each file's diff the moment the cursor lands on it, with nothing left to
/// wait for: the rows are always those of the file under the cursor.
#[test]
fn a_held_key_shows_each_file_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let [start, first, second] = three_files(host.model());
    let start_path = path_at(host.model(), start);
    host.update(|model| model.show_file(&start_path));
    for position in [first, second] {
        host.dispatch(Action::TreeDown);
        let path = path_at(host.model(), position);
        assert_eq!(host.model().selected.as_deref(), Some(path.as_str()));
        assert_eq!(host.model().rows.files[0].path, path);
        assert_ne!(host.model().rows.rows.len(), 0, "the diff is built at once");
    }
    assert!(
        host.timers.is_empty(),
        "nothing waits for the cursor to rest"
    );
}

/// Moving quickly to the files around one and back returns to the hunk and the row left in it,
/// and a mark made right away lands on the hunk that shows.
#[test]
fn a_quick_round_trip_returns_to_the_place_left_and_marks_what_shows() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let engine = "src/engine.rs";
    host.update(|model| model.show_file(engine));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    // It opens at its last hunk, the first that needs the reviewer; a step back and forth
    // stays within it.
    host.dispatch(Action::PreviousHunk);
    host.dispatch(Action::NextHunk);
    lay_out(&mut host);
    let hunk = host.model().hunk;
    assert!(hunk > 0, "the engine has hunks to move through");
    let units = host.model().hunk_units();
    let top = host.model().diff_scroll.target();

    // B → C → B → A → B, faster than any rest.
    host.dispatch(Action::FocusTree);
    for action in [
        Action::TreeDown,
        Action::TreeUp,
        Action::TreeUp,
        Action::TreeDown,
    ] {
        host.dispatch(action);
        lay_out(&mut host);
    }
    let model = host.model();
    assert_eq!(model.selected.as_deref(), Some(engine));
    assert_eq!(model.rows.files[0].path, engine);
    assert_eq!(model.hunk, hunk);
    assert_eq!(model.hunk_units(), units);
    assert!((model.diff_scroll.target() - top).abs() < f64::EPSILON);

    // The mark lands on that hunk.
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    let saved = host.run_job();
    host.complete(saved);
    let view = host.model().view().unwrap();
    assert!(
        state::badges(&view, &units, &host.model().pending)
            .into_iter()
            .all(state::is_approved),
        "the hunk that showed is approved"
    );
}

/// Moving on while a mark is saved shows the next file at once, at its first hunk that needs the
/// reviewer, and the mark landing keeps it there.
#[test]
fn a_mark_landing_after_moving_on_keeps_the_next_file_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let review = demo(dir.path());

    // The engine's first display hunk is approved beforehand, so its first open hunk is a later
    // one.
    let engine = "src/engine.rs";
    {
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let units = view.units_of_file(engine);
        let folds = rows::Folds {
            single_file: true,
            ..rows::Folds::default()
        };
        let hunks = rows::present(
            rows::build(&crate::rows::Source::Verified(view), &units, &folds),
            Presentation::default(),
        )
        .hunks;
        assert!(hunks.len() > 2, "the engine has several hunks");
        review
            .mark(&view, &[hunks[0].unit], MarkVerdict::Accepted)
            .unwrap();
    }
    let mut host = Host::open(review);

    // Another file shows, and its first hunk is marked; the mark waits.
    let other = file_paths(&host.model().tree)
        .into_iter()
        .find(|path| *path != engine)
        .unwrap()
        .to_owned();
    host.update(|model| model.show_file(&other));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    assert_eq!(host.jobs.len(), 1, "the mark is being saved");

    // Back in the tree, the cursor moves on to the engine, which shows at once.
    host.dispatch(Action::FocusTree);
    let engine_position = host
        .model()
        .visible_tree()
        .iter()
        .position(|&index| host.model().tree[index].path == engine)
        .unwrap();
    host.update(|model| model.tree_cursor = engine_position - 1);
    host.dispatch(Action::TreeDown);
    assert_eq!(host.model().selected.as_deref(), Some(engine));
    assert_eq!(host.model().rows.files[0].path, engine);
    let opened_at = host.model().hunk;

    // The mark lands: the tree takes it in, and the engine stays at the hunk it opened at.
    let saved = host.run_job();
    host.complete(saved);
    assert!(!host.model().saving);
    let model = host.model();
    assert_eq!(model.rows.files[0].path, engine);
    assert_eq!(model.hunk, opened_at);
    let view = model.view().unwrap();
    let approved = |hunk: usize| {
        crate::state::badges(&view, &[model.rows.hunks[hunk].unit], &model.pending)
            .into_iter()
            .all(crate::state::is_approved)
    };
    assert!(model.hunk > 0);
    assert!(
        (0..model.hunk).all(approved),
        "every hunk before is approved"
    );
    assert!(
        !approved(model.hunk),
        "the selected hunk needs the reviewer"
    );
}

/// A picker's searches land only while they are its newest: an older one that ends last never
/// replaces the newer one's entries, and Enter during a search opens what the newest search
/// finds, never what an older one found.
#[test]
fn only_the_newest_picker_search_lands() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::FindFiles);
    host.settle();

    // Two queries; the newer search ends first, then the older one.
    host.update(|model| model.edit_field(Field::PickerQuery, "rows"));
    host.update(|model| model.edit_field(Field::PickerQuery, "engine"));
    assert_eq!(host.jobs.len(), 2);
    let newer = host.run_newest_job();
    host.complete(newer);
    let older = host.run_job();
    host.complete(older);
    let picker = host.model().picker.as_ref().unwrap();
    assert!(!picker.searching);
    assert_eq!(picker.entries[0].path, "src/engine.rs");

    // Echoing the same text back starts no search.
    host.update(|model| model.edit_field(Field::PickerQuery, "engine"));
    assert!(host.jobs.is_empty());

    // Enter while a search runs waits for it; a newer query supersedes that search.
    host.update(|model| model.edit_field(Field::PickerQuery, "rows"));
    host.update(|model| model.submit_field(Field::PickerQuery));
    host.update(|model| model.edit_field(Field::PickerQuery, "engine"));
    let older = host.run_job();
    host.complete(older);
    assert!(
        host.model().picker.is_some(),
        "the older result opens nothing"
    );
    let newer = host.run_job();
    host.complete(newer);
    assert!(
        host.model().picker.is_none(),
        "the newest result opens its first entry"
    );
    assert_eq!(host.model().selected.as_deref(), Some("src/engine.rs"));
    assert_eq!(host.model().mode, Mode::Diff);

    // The searches of a picker since closed never land in a newer picker.
    host.dispatch(Action::FindFiles);
    host.update(|model| model.edit_field(Field::PickerQuery, "engine"));
    host.dispatch(Action::PickerClose);
    host.dispatch(Action::FindFiles);
    let closed_opening = host.run_job();
    let closed_query = host.run_job();
    let opening = host.run_job();
    host.complete(opening);
    let listed = host.model().picker.as_ref().unwrap().entries.len();
    assert!(listed > 1, "the new picker lists every changed file");
    host.complete(closed_opening);
    host.complete(closed_query);
    let picker = host.model().picker.as_ref().unwrap();
    assert_eq!(picker.query, "");
    assert_eq!(picker.entries.len(), listed);
}

/// The content search waits for typing to pause, and a pause that a newer keystroke cut short
/// starts no search.
#[test]
fn the_content_search_waits_for_a_pause() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::SearchContent);
    host.settle();
    host.update(|model| model.edit_field(Field::PickerQuery, "bal"));
    host.update(|model| model.edit_field(Field::PickerQuery, "balance"));
    assert!(host.jobs.is_empty(), "nothing searches while typing");
    assert!(host.model().picker.as_ref().unwrap().searching);
    host.fire(0);
    assert!(host.jobs.is_empty(), "the first pause was cut short");
    host.fire(0);
    let found = host.run_job();
    host.complete(found);
    let picker = host.model().picker.as_ref().unwrap();
    assert!(!picker.searching);
    assert_ne!(picker.entries.len(), 0, "the content search finds the name");
    assert!(
        picker
            .entries
            .iter()
            .all(|entry| entry.text.to_lowercase().contains("balance"))
    );
}

/// The central check drops what an older search of the open picker found: its failure never
/// shows, and the newest search still runs.
#[test]
fn an_older_search_is_dropped_centrally() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::FindFiles);
    host.settle();
    host.update(|model| model.edit_field(Field::PickerQuery, "engine"));
    let newest = host.model().picker_generation().unwrap();
    let failed = |generation| Completion {
        subject: Subject::Search(generation),
        outcome: Box::new(Outcome::Found(Found {
            entries: Vec::new(),
            truncated: false,
            listing: None,
            error: Some("an older search failed".to_owned()),
        })),
    };

    host.complete(failed(newest - 1));
    let picker = host.model().picker.as_ref().unwrap();
    assert_eq!(picker.error, None);
    assert!(picker.searching, "the newest search still runs");

    host.complete(failed(newest));
    let picker = host.model().picker.as_ref().unwrap();
    assert_eq!(picker.error.as_deref(), Some("an older search failed"));
    assert!(!picker.searching);
}

/// The central check drops a preview's text read for a picker since closed, so it never lands
/// in the picker opened after it.
#[test]
fn a_preview_read_for_a_closed_picker_is_dropped_centrally() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::SearchContent);
    let closed = host.model().picker.as_ref().unwrap().id;
    host.dispatch(Action::PickerClose);
    host.dispatch(Action::SearchContent);
    host.settle();
    let open = host.model().picker.as_ref().unwrap().id;
    assert_ne!(open, closed);
    // A file that could not be read, which a landing read records.
    let unreadable = |id| Completion {
        subject: Subject::Picker(id),
        outcome: Box::new(Outcome::PreviewText(PreviewText {
            path: "assets/logo.bin".to_owned(),
            text: None,
        })),
    };

    host.complete(unreadable(closed));
    let picker = host.model().picker.as_ref().unwrap();
    assert!(!picker.unreadable.contains("assets/logo.bin"));

    host.complete(unreadable(open));
    let picker = host.model().picker.as_ref().unwrap();
    assert!(picker.unreadable.contains("assets/logo.bin"));
}

/// With language servers turned off, go to definition says so and starts nothing, even in a
/// trusted checkout.
#[test]
fn go_to_definition_is_off_without_language_servers() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host_with(
        dir.path(),
        Settings {
            language_servers: false,
            ..settings()
        },
    );
    trust(&mut host);
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    let model = host.model();
    assert!(!model.looking_up);
    assert!(host.jobs.is_empty(), "no lookup runs");
    assert_eq!(model.parts().servers.len(), 0);
    assert!(
        model.status.contains("[lsp] enabled = false"),
        "{}",
        model.status
    );
}

/// The central check drops a definition once the review is no longer trusted, saying nothing,
/// though the lookup still ends.
#[test]
fn an_untrusted_lookup_is_dropped_centrally() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    trust(&mut host);
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    assert!(host.model().looking_up);
    let asked = lookup(host.model());
    host.dispatch(Action::ToggleTrust);
    assert_eq!(
        host.model().trust,
        Trust::Checking,
        "the revocation is under way"
    );
    let status = host.model().status.clone();

    host.complete(found_for(asked));
    let model = host.model();
    assert!(!model.looking_up);
    assert!(model.buffer.as_ref().unwrap().previous.is_none());
    assert_eq!(model.status, status, "the dropped answer says nothing");
}

/// The central check drops a file read to open as a buffer for another snapshot than shows:
/// no buffer opens, though the read still ends; one read for the snapshot that shows opens.
#[test]
fn a_file_read_for_another_revision_is_dropped_centrally() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let shown = host.model().shown;
    let other = shown + 1;
    let read = |generation| Completion {
        subject: Subject::Shown(generation),
        outcome: Box::new(Outcome::Unchanged(Unchanged {
            path: "README.md".to_owned(),
            line: 0,
            column: 0,
            text: Some(rows::FullText::new("# Ledger\n")),
            syntax: None,
        })),
    };

    host.update(|model| model.highlighting = 1);
    host.complete(read(other));
    assert_eq!(host.model().highlighting, 0, "the read ended");
    assert!(host.model().buffer.is_none(), "no buffer opens");

    host.update(|model| model.highlighting = 1);
    host.complete(read(shown));
    assert_eq!(host.model().mode, Mode::Buffer);
    assert_eq!(host.model().buffer.as_ref().unwrap().title(), "README.md");
}

/// Trusts the checkout, which no checkout is until the reviewer decides, as `Ctrl+T` and the
/// trust question's `c` do, and waits for the grant to be recorded.
fn trust(host: &mut Host) {
    host.dispatch(Action::ToggleTrust);
    assert_eq!(host.model().overlay, Some(Overlay::Trust));
    host.dispatch(Action::TrustCheckout);
    host.settle();
    assert!(host.model().trust.granted(), "{}", host.model().status);
}

/// Stands a process in for the language server `program` that a lookup of the window started
/// in `root` just now, which runs until it is stopped; returns the server as the lookup holds
/// it, and its process.
#[cfg(target_os = "linux")]
fn serve(
    host: &mut Host,
    program: &'static str,
    root: &Path,
) -> (Arc<parking_lot::Mutex<crate::lsp::LanguageServer>>, u32) {
    let server = crate::lsp::LanguageServer::stand_in("cat", &[], root).unwrap();
    let process = server.process_id();
    (
        host.model().parts().servers.insert(program, server),
        process,
    )
}

/// Opens `a.rs` in the buffer with the cursor on a name, ready for a lookup.
fn buffer_on_a_name(host: &mut Host) {
    host.update(|model| model.show_file("a.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleBuffer);
    host.dispatch(Action::NextWord);
    assert_eq!(host.model().mode, Mode::Buffer);
}

/// The lookup the model is running now, as its completion will name it.
fn lookup(model: &Model) -> Lookup {
    Lookup {
        buffer: model.buffer.as_ref().unwrap().id,
        shown: model.shown,
        epoch: model.epoch(),
    }
}

/// The completion of a definition lookup `asked` that found `definition()`.
fn found_for(asked: Lookup) -> Completion {
    Completion {
        subject: Subject::Lookup(asked),
        outcome: Box::new(Outcome::Definition(Ok(definition()))),
    }
}

/// A definition in another file of the review.
fn definition() -> Definition {
    Definition {
        path: "src/ledger.rs".to_owned(),
        line: 1,
        column: 0,
        lines: rows::FullText::new(indoc! {"
            // ledger
            fn balance() {}
        "}),
        syntax: crate::syntax::analyze(
            "src/ledger.rs",
            indoc! {"
                // ledger
                fn balance() {}
            "},
        ),
    }
}

/// A definition that arrives after trust was revoked is dropped, and the server the lookup
/// used stops once the lookup lets go of it.
#[cfg(target_os = "linux")]
#[test]
fn a_definition_after_trust_is_revoked_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    trust(&mut host);
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    assert!(host.model().looking_up);
    assert_eq!(host.jobs.len(), 1, "the lookup runs in the background");
    let asked = lookup(host.model());
    let buffer = host.model().buffer.as_ref().unwrap().id;

    // A language server runs, which the lookup uses while it waits for the answer.
    let (server, process) = serve(&mut host, "rust", dir.path());
    let in_use = server.lock();

    // Trust is revoked while the language server answers: by the time the window says so, the
    // revocation has stopped every server, the one in use included, and its process has exited
    // and is reaped, though the lookup still holds the server.
    host.dispatch(Action::ToggleTrust);
    assert_eq!(host.model().status, "Revoking trust…");
    let revoked = host.run_newest_job();
    host.complete(revoked);
    assert_eq!(host.model().trust, Trust::Denied);
    assert!(
        host.model()
            .status
            .starts_with("Trust in this checkout revoked"),
        "{}",
        host.model().status
    );
    assert_eq!(host.model().parts().servers.len(), 0);
    assert!(
        !Path::new(&format!("/proc/{process}")).exists(),
        "the server's process {process} still runs"
    );
    drop(in_use);
    drop(server);
    let status = host.model().status.clone();
    host.complete(found_for(asked));

    let model = host.model();
    assert!(!model.looking_up);
    assert_eq!(
        model.buffer.as_ref().unwrap().id,
        buffer,
        "the buffer stays"
    );
    assert!(model.buffer.as_ref().unwrap().previous.is_none());
    assert_eq!(model.status, status, "the answer says nothing");
}

/// A definition lands only in the buffer and revision it was asked for: an answer for a buffer
/// since replaced, or for another revision, is dropped, and one for what shows moves there.
#[test]
fn a_definition_lands_only_where_it_was_asked() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    trust(&mut host);
    buffer_on_a_name(&mut host);

    // The buffer is left and opened again while the lookup runs: another buffer now.
    host.dispatch(Action::GoToDefinition);
    let asked = lookup(host.model());
    host.dispatch(Action::ToggleBuffer);
    host.dispatch(Action::ToggleBuffer);
    let shown = host.model().buffer.as_ref().unwrap().id;
    assert_ne!(shown, asked.buffer);
    host.complete(found_for(asked));
    let model = host.model();
    assert_eq!(model.buffer.as_ref().unwrap().id, shown);
    assert!(model.buffer.as_ref().unwrap().previous.is_none());
    assert_eq!(
        model.status,
        "The definition lookup finished for a file no longer shown."
    );

    // An answer for another snapshot of the shown buffer is dropped too.
    let current = lookup(host.model());
    let other = Lookup {
        shown: current.shown + 1,
        ..current
    };
    host.update(|model| model.looking_up = true);
    host.complete(found_for(other));
    assert!(host.model().buffer.as_ref().unwrap().previous.is_none());
    assert!(
        !host.model().looking_up,
        "a dropped answer still ends the lookup"
    );

    // The answer for what shows moves to the definition, which Ctrl+O leaves.
    host.complete(found_for(current));
    let buffer = host.model().buffer.as_ref().unwrap();
    assert_eq!(buffer.title(), "src/ledger.rs");
    assert_eq!(buffer.cursor.row, 1);
    assert!(buffer.previous.is_some());
}

/// Quitting waits for the marks still queued: three marks made in a burst and a quit right
/// after them are all recorded before the app quits, the reset among them too, and the status
/// line says what the app waits for.
#[test]
fn quitting_waits_for_queued_marks() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    // The engine's last two hunks are approved, the third press resetting the last again,
    // which is the mark whose loss would leave code approved.
    for _ in 0..3 {
        host.dispatch(Action::ToggleApprove);
    }
    assert_eq!(host.model().saves.len(), 2, "one mark saves, two wait");
    let wanted = host.model().pending.clone();
    assert!(
        wanted
            .values()
            .any(|&verdict| verdict == MarkVerdict::Unseen)
    );

    host.dispatch(Action::Quit);
    assert_eq!(host.quits, 0, "nothing quits while marks wait");
    assert_eq!(
        host.model().status,
        "Saving 3 changes to the review before quitting…"
    );

    // Each saved mark starts the next; the last one quits, once.
    let saved = host.run_job();
    host.complete(saved);
    assert_eq!(host.quits, 0);
    assert_eq!(
        host.model().status,
        "Saving 2 changes to the review before quitting…"
    );
    host.settle();
    assert_eq!(host.quits, 1);
    assert_eq!(host.model().quitting, Quitting::Done);

    // The review recorded what the reviewer saw: the last verdict of each unit.
    assert_eq!(recorded(&host.review, &wanted), wanted);
}

/// The verdicts the review recorded for the units of `wanted`, as the marks name them: an
/// approval, or a reset for a unit nobody reviewed.
fn recorded(
    review: &Review,
    wanted: &HashMap<UnitId, MarkVerdict>,
) -> HashMap<UnitId, MarkVerdict> {
    let history = review.history().unwrap();
    let view = history.current_view().unwrap();
    wanted
        .keys()
        .map(|&unit| {
            let verdict = match view.state(unit).unwrap() {
                review_core::UnitState::Reviewed {
                    verdict: review_core::Verdict::Accepted,
                    ..
                } => MarkVerdict::Accepted,
                review_core::UnitState::Reviewed {
                    verdict: review_core::Verdict::Flagged,
                    ..
                } => MarkVerdict::Flagged,
                _ => MarkVerdict::Unseen,
            };
            (unit, verdict)
        })
        .collect()
}

/// While the app waits to quit it takes no new marks: a mark key says so, the note stays on
/// the status line, and only the mark made before quitting is recorded.
#[test]
fn marks_are_refused_while_quitting_waits() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    let units: HashMap<UnitId, MarkVerdict> = host
        .model()
        .rows
        .hunks
        .iter()
        .map(|hunk| (hunk.unit, MarkVerdict::Unseen))
        .collect();
    let before = recorded(&host.review, &units);
    host.dispatch(Action::ToggleApprove);
    let wanted = host.model().pending.clone();
    host.dispatch(Action::Quit);

    // The next hunk's approval, a flag and a reset are all refused.
    for action in [
        Action::ToggleApprove,
        Action::ToggleFlag,
        Action::ResetReview,
    ] {
        host.dispatch(action);
        assert!(host.model().saves.is_empty(), "{action:?} queued nothing");
        assert_eq!(host.model().pending, wanted, "{action:?} changed nothing");
        assert_eq!(
            host.model().status,
            "Not marked while quitting. Saving 1 change to the review before quitting…"
        );
    }

    // Only the mark made before quitting is recorded.
    host.settle();
    assert_eq!(host.quits, 1);
    let mut expected = before;
    expected.extend(wanted);
    assert_eq!(recorded(&host.review, &units), expected);
}

/// Quitting again while marks wait, as a held key or a double click on the close button does,
/// drops nothing: every approval made before is recorded, and the app leaves once they are.
#[test]
fn a_second_quit_while_waiting_drops_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/parser.rs"));
    host.dispatch(Action::FocusDiff);
    for _ in 0..3 {
        host.dispatch(Action::ToggleApprove);
    }
    assert_eq!(host.model().saves.len(), 2, "one mark saves, two wait");
    let wanted = host.model().pending.clone();
    assert!(
        wanted
            .values()
            .all(|&verdict| verdict == MarkVerdict::Accepted)
    );

    host.dispatch(Action::Quit);
    host.dispatch(Action::Quit);
    let model = host.model();
    assert_eq!(model.quitting, Quitting::Waiting(QuitKind::Interactive));
    assert_eq!(model.saves.len(), 2, "both approvals still wait");
    assert_eq!(
        model.pending, wanted,
        "every approval still shows as pending"
    );
    assert_eq!(
        model.status,
        "Saving 3 changes to the review before quitting…"
    );
    assert_eq!(host.quits, 0, "the second quit leaves no sooner");

    host.settle();
    assert_eq!(host.quits, 1);
    assert_eq!(recorded(&host.review, &wanted), wanted);
}

/// Makes every file and directory of the review data at `data` read-only, or writable again,
/// so the engine fails to record marks.
#[cfg(unix)]
fn set_writable(data: &Path, writable: bool) {
    use std::os::unix::fs::PermissionsExt as _;

    for path in walk(data) {
        let mode = match (path.is_dir(), writable) {
            (true, true) => 0o755,
            (true, false) => 0o555,
            (false, true) => 0o644,
            (false, false) => 0o444,
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

/// The review data of the host's open review.
#[cfg(unix)]
fn data_dir(host: &Host) -> std::path::PathBuf {
    host.review.dir().root().to_path_buf()
}

/// A mark that cannot be saved stops the quitting, so the failure stays on screen; quitting
/// again leaves.
#[cfg(unix)]
#[test]
fn a_failed_mark_stops_quitting() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::Quit);

    // The review's data cannot be written, so the mark fails.
    let data = data_dir(&host);
    set_writable(&data, false);
    host.settle();
    set_writable(&data, true);

    assert_eq!(host.quits, 0, "the failure keeps the app open");
    assert_eq!(host.model().quitting, Quitting::Stopped);
    assert!(
        host.model().status.starts_with("Could not record the mark")
            && host
                .model()
                .status
                .ends_with("Quitting stopped; quit again to leave anyway."),
        "{}",
        host.model().status
    );
    host.dispatch(Action::Quit);
    host.settle();
    assert_eq!(host.quits, 1, "a second quit leaves");
}

/// A mark made after a failure stopped the quitting is the reviewer reviewing again: the next
/// quit is an ordinary one, which waits for that mark and which a failure would stop again.
#[cfg(unix)]
#[test]
fn a_mark_after_a_stopped_quit_makes_the_next_quit_ordinary() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::Quit);
    let data = data_dir(&host);
    set_writable(&data, false);
    host.settle();
    set_writable(&data, true);
    assert_eq!(host.model().quitting, Quitting::Stopped);

    host.dispatch(Action::ToggleApprove);
    assert_eq!(host.model().quitting, Quitting::No);
    let wanted = host.model().pending.clone();
    host.dispatch(Action::Quit);
    assert_eq!(
        host.model().quitting,
        Quitting::Waiting(QuitKind::Interactive)
    );
    host.settle();
    assert_eq!(host.quits, 1);
    assert_eq!(recorded(&host.review, &wanted), wanted);
}

/// However many queued marks fail, the reviewer quits with exactly two quits: the first stops
/// at the first failure, and the second leaves once every mark still queued is tried, whether
/// it comes after the last failure or while marks still wait, and no later failure stops it.
#[cfg(unix)]
#[test]
fn failing_queued_marks_need_exactly_two_quits() {
    // The second quit after every mark failed
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    for _ in 0..3 {
        host.dispatch(Action::ToggleApprove);
    }
    host.dispatch(Action::Quit);
    let data = data_dir(&host);
    set_writable(&data, false);
    host.settle();
    set_writable(&data, true);
    assert_eq!(host.quits, 0);
    assert_eq!(host.model().quitting, Quitting::Stopped);
    host.dispatch(Action::Quit);
    host.settle();
    assert_eq!(host.quits, 1);

    // The second quit while an approval saves and another waits, three hunks approved
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/parser.rs"));
    host.dispatch(Action::FocusDiff);
    for _ in 0..3 {
        host.dispatch(Action::ToggleApprove);
    }
    assert_eq!(host.model().saves.len(), 2);
    host.dispatch(Action::Quit);
    let data = data_dir(&host);
    set_writable(&data, false);
    let failed = host.run_job();
    host.complete(failed);
    assert_eq!(host.model().quitting, Quitting::Stopped);
    assert!(
        host.model().status.ends_with(
            "Quitting stopped; quit again to leave once the marks still queued are saved."
        ),
        "{}",
        host.model().status
    );
    host.dispatch(Action::Quit);
    assert_eq!(
        host.model().quitting,
        Quitting::Waiting(QuitKind::Unattended)
    );
    assert_eq!(
        host.model().saves.len(),
        1,
        "the approval still queued is kept"
    );
    assert_eq!(host.quits, 0, "the mark being saved comes first");
    // The next mark fails too, which no longer stops the quitting, and so does the last; the
    // app leaves once both were tried.
    let failed = host.run_job();
    host.complete(failed);
    assert_eq!(
        host.model().quitting,
        Quitting::Waiting(QuitKind::Unattended)
    );
    assert_eq!(host.quits, 0, "the last approval is still tried");
    host.settle();
    set_writable(&data, true);
    assert_eq!(host.quits, 1);
    let failures = host
        .logged
        .iter()
        .filter(|message| message.starts_with("could not record the mark"))
        .count();
    assert_eq!(failures, 3, "every mark was tried: {:?}", host.logged);
}

/// A quit nobody can see, such as a hang-up's, is never stopped by a failed mark: every queued
/// mark is still tried, the log keeps each failure, and the app leaves once they settle.
#[cfg(unix)]
#[test]
fn an_unattended_quit_is_not_cancelled_by_a_failed_mark() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::ToggleApprove);
    host.update(|model| model.quit(QuitKind::Unattended));

    let data = data_dir(&host);
    set_writable(&data, false);
    host.settle();
    set_writable(&data, true);

    assert_eq!(host.quits, 1);
    assert_eq!(host.model().quitting, Quitting::Done);
    let failures = host
        .logged
        .iter()
        .filter(|message| message.starts_with("could not record the mark"))
        .count();
    assert_eq!(failures, 2, "{:?}", host.logged);

    // An unattended quit also keeps a reviewer's stopped quit from waiting for anyone.
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::Quit);
    let data = data_dir(&host);
    set_writable(&data, false);
    let failed = host.run_job();
    host.complete(failed);
    assert_eq!(host.model().quitting, Quitting::Stopped);
    host.update(|model| model.quit(QuitKind::Unattended));
    host.settle();
    set_writable(&data, true);
    assert_eq!(host.quits, 1);
}

/// Every file and directory under `root`, `root` first.
#[cfg(unix)]
fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut found = vec![root.to_path_buf()];
    let mut index = 0;
    while let Some(path) = found.get(index).cloned() {
        index += 1;
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                found.push(entry.unwrap().path());
            }
        }
    }
    found
}

/// Folded folders save one at a time: a change while a save runs waits for it, then saves the
/// newest folds.
#[test]
fn folder_saves_never_overtake_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::CloseAllFolds);
    assert_eq!(host.jobs.len(), 1, "the folds are saved");
    host.dispatch(Action::OpenAllFolds);
    assert_eq!(
        host.jobs.len(),
        1,
        "the next save waits for the running one"
    );
    let saved = host.run_job();
    host.complete(saved);
    assert_eq!(host.jobs.len(), 1, "the newest folds are saved next");
    host.settle();
    let review = host.review.clone();
    assert_eq!(
        crate::folders::load(&review).map(|state| state.collapsed),
        Ok(Vec::<String>::new())
    );
}

/// Quitting saves where the reviewer was, and the review opens there again, at the same file
/// and hunk; moving around saves nothing. A saved file the revision does not have opens the
/// review as the first time, and a saved unit the file does not have opens the file at its
/// first hunk that needs the reviewer.
#[test]
fn the_review_opens_where_the_reviewer_quit() {
    let dir = tempfile::tempdir().unwrap();
    let reopen = || {
        prepare(
            &DemoPaths::under(dir.path()),
            &fixture_dir(),
            DemoRequest::UpTo(RevisionNumber::new(1).unwrap()),
        )
        .unwrap()
    };
    let mut host = Host::open(demo(dir.path()));
    let first = host.model().selected.clone();
    host.update(|model| model.show_file("server.go"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::FirstHunk);
    for _ in 0..5 {
        host.dispatch(Action::NextHunk);
    }
    assert_eq!(host.model().hunk, 5);
    assert!(host.jobs.is_empty(), "moving around saves nothing");
    host.dispatch(Action::Quit);
    host.settle();
    assert_eq!(host.quits, 1);

    let host = Host::open(reopen());
    assert_eq!(host.model().selected.as_deref(), Some("server.go"));
    assert_eq!(host.model().hunk, 5);

    let review = reopen();
    let save = |place: crate::folders::Place| {
        review
            .save_view_state(
                crate::folders::FRONT_END,
                &crate::folders::ViewState {
                    collapsed: Vec::new(),
                    place: Some(place),
                },
            )
            .unwrap();
    };
    save(crate::folders::Place {
        path: "gone.rs".to_owned(),
        unit: None,
    });
    let host = Host::open(reopen());
    assert_eq!(host.model().selected, first);

    save(crate::folders::Place {
        path: "src/engine.rs".to_owned(),
        unit: Some("u1-9999".to_owned()),
    });
    let host = Host::open(reopen());
    assert_eq!(host.model().selected.as_deref(), Some("src/engine.rs"));
    assert_eq!(
        host.model().hunk,
        2,
        "at its first hunk that needs the reviewer"
    );
}

/// Approved code loses its diff colors, its signs aside, its removed lines fading as code that
/// is gone, and sits on the hunks' gray band, while code still to review keeps them, an edited
/// line on its soft background, and has a bar too, and a mark on the ruler; and in the bold
/// style only the selected hunk keeps bold line backgrounds, the others soft ones, with a bar
/// across the frame against a sliver, and line numbers in the text color against quiet ones.
#[test]
fn approved_code_reads_as_code_and_the_selected_hunk_stands_out() {
    use crate::paint::Palette;
    use crate::paint::code::{At, CodeCell, Frame, code_cell, frame, ruler};
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    // The changed lines of `hunk`, painted.
    let changed = |model: &Model, hunk: usize| -> Vec<(rows::RowKind, CodeCell, Frame)> {
        let palette = Palette::of(model);
        model.hunk_rows[hunk]
            .clone()
            .filter_map(|index| {
                let row = &model.rows.rows[index];
                let cell = row.cells().next()?;
                matches!(
                    cell.kind,
                    rows::RowKind::Added | rows::RowKind::Removed | rows::RowKind::Edited
                )
                .then(|| {
                    let at = At {
                        row: index,
                        file: row.file,
                        column: Column::Whole,
                    };
                    (
                        cell.kind,
                        code_cell(model, &palette, at, cell),
                        frame(model, index),
                    )
                })
            })
            .collect()
    };
    host.dispatch(Action::FirstHunk);
    let model = host.model();
    let palette = Palette::of(model);
    let approved = changed(model, 0);
    assert_ne!(approved.len(), 0, "the first hunk has changed lines");
    for (kind, cell, frame) in &approved {
        assert_eq!(
            cell.background, None,
            "approved code has no line background"
        );
        assert_eq!(
            frame.band(&palette),
            Some(palette.selected_band),
            "approved code is banded, brighter while selected"
        );
        assert_ne!(cell.sign.text, " ", "its sign stays");
        let faded = (*kind == rows::RowKind::Removed).then_some(palette.faint);
        assert_eq!(cell.text_color, faded);
    }
    let soft = |line: crate::color::Hsla| {
        crate::theme::mix(line, palette.background, crate::look::MUTED_SHARE)
    };
    let line = |kind: rows::RowKind| match kind {
        rows::RowKind::Removed => Some(palette.removed_line),
        rows::RowKind::Added => Some(palette.added_line),
        rows::RowKind::Edited => Some(palette.edited_line),
        _ => None,
    };
    for (kind, cell, frame) in changed(model, 2) {
        assert_eq!(
            cell.background,
            line(kind).map(soft),
            "a hunk not selected is soft"
        );
        assert_eq!(frame.band(&palette), None, "open code has no band");
        assert!(frame.bar(&palette).is_some(), "open code has a bar too");
        assert!(frame.bar_share() < 1.0);
        if kind != rows::RowKind::Removed {
            assert_eq!(cell.number.color, palette.faint);
        }
    }
    // The ruler marks where code still needs the reviewer, and not where it is approved.
    let rows = crate::scroll::float(model.rows.rows.len());
    let marked = |hunk: usize| {
        let start = crate::scroll::float(model.hunk_rows[hunk].start) / rows;
        ruler(model, &palette)
            .iter()
            .any(|mark| (mark.start - start).abs() < f32::EPSILON)
    };
    assert!(marked(2) && !marked(0));
    host.dispatch(Action::LastHunk);
    let model = host.model();
    for (kind, cell, frame) in changed(model, 2) {
        assert_eq!(cell.background, line(kind), "the selected hunk is bold");
        assert!((frame.bar_share() - 1.0).abs() < f32::EPSILON);
        if kind != rows::RowKind::Removed {
            assert_eq!(cell.number.color, palette.text);
        }
    }
    for (_, _, frame) in changed(model, 0) {
        assert_eq!(frame.band(&palette), Some(palette.band));
    }
}

/// The file view shows moved code at both ends of a move, in the move's color: the moved lines
/// where they left, under their note, and where they arrived the lines the move kept, with its
/// edits still removed and added.
#[test]
fn moved_code_shows_in_the_move_color() {
    use crate::paint::Palette;
    use crate::paint::code::{At, code_cell};
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    let painted = |model: &Model| -> Vec<(rows::RowKind, bool, Option<crate::color::Hsla>)> {
        let palette = Palette::of(model);
        model
            .rows
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                let cell = row.cells().next()?;
                let at = At {
                    row: index,
                    file: row.file,
                    column: Column::Whole,
                };
                Some((
                    cell.kind,
                    cell.moved,
                    code_cell(model, &palette, at, cell).background,
                ))
            })
            .collect()
    };
    host.update(|model| model.show_file("src/report.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let model = host.model();
    let palette = Palette::of(model);
    let source = painted(model);
    assert!(
        source.iter().filter(|(_, moved, _)| *moved).count() >= 10,
        "the moved lines show where they left"
    );
    for (kind, moved, background) in &source {
        if *moved {
            assert_eq!(*kind, rows::RowKind::Removed);
            assert_eq!(*background, Some(palette.moved_line));
        }
    }

    host.update(|model| model.show_file("src/format.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let model = host.model();
    let target = painted(model);
    let kept = target
        .iter()
        .filter(|(kind, moved, _)| *moved && *kind == rows::RowKind::NestedSame)
        .count();
    assert!(kept >= 8, "the lines the move kept show in its color");
    assert!(
        target.iter().any(|(kind, moved, background)| *moved
            && *kind == rows::RowKind::NestedNew
            && *background == Some(palette.added_line)),
        "its edits stay added"
    );
}

/// The demo's `line_error` leaves `mod config` for the file's top level, and its
/// `occupancy_line` ends up in `mod worker`, each where it stands as a module's closing brace
/// moves past it: re-indented in place, which is no move. Neither shows a move's note or color,
/// and each of their lines shows once, edited in place.
#[test]
fn a_rust_reindent_in_place_is_no_move() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("scheduler.rs"));
    lay_out(&mut host);
    let model = host.model();
    let notes: Vec<String> = model
        .rows
        .rows
        .iter()
        .filter(|row| row.kind() == rows::RowKind::Moved)
        .map(|row| row.text().to_owned())
        .collect();
    assert!(notes.is_empty(), "{notes:?}");
    // Re-indented where they stand, every line shows once, as the code it now is.
    for text in ["reason: reason.into()", "still free"] {
        let shown: Vec<(rows::RowKind, bool)> = model
            .rows
            .rows
            .iter()
            .filter_map(|row| row.cells().next())
            .filter(|cell| cell.text.contains(text))
            .map(|cell| (cell.kind, cell.moved))
            .collect();
        assert_eq!(shown, [(rows::RowKind::Edited, false)], "{text}");
    }
}

/// The demo's Python `spread`, moved into `main`, becomes a function of `main` there, so the
/// engine keeps its edits: both ends say it moved, re-indented four deeper, and where it arrives
/// each of its lines shows once, the indentation it gained marked.
#[test]
fn a_python_reindent_shows_each_line_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("tools/stats.py"));
    lay_out(&mut host);
    let model = host.model();
    let notes: Vec<String> = model
        .rows
        .rows
        .iter()
        .filter(|row| row.kind() == rows::RowKind::Moved)
        .map(|row| row.text().to_owned())
        .collect();
    for arrow in ["↗ moved to lines ", "↙ moved from lines "] {
        assert!(
            notes.iter().any(|note| note.starts_with(arrow)
                && note.contains(" of this file, re-indented 4 deeper")),
            "{notes:?}"
        );
    }
    let shown: Vec<(rows::RowKind, Vec<std::ops::Range<usize>>)> = model
        .rows
        .rows
        .iter()
        .filter_map(|row| row.cells().next())
        .filter(|cell| cell.moved && cell.text.contains("return max(values) - min(values)"))
        .map(|cell| (cell.kind, cell.highlights.clone()))
        .collect();
    assert_eq!(
        shown,
        [
            (rows::RowKind::Removed, Vec::new()),
            (rows::RowKind::NestedSame, std::iter::once(4..8).collect()),
        ],
        "the source's line, then the line where it arrived with its gained indentation marked"
    );
}

/// A copy hands the selected text to the clipboard and the primary selection, while a drag
/// only selects; a click that selects nothing copies nothing.
#[test]
fn selections_reach_the_clipboards() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let (row, line) = engine_line(&mut host);
    host.update(|model| model.press_text(text_press(row, 0)));
    host.update(|model| model.drag_text(Pane::Diff, TextPos { row, col: 4 }));
    host.update(Model::release_text);
    assert!(host.copied.is_empty() && host.primary.is_empty());

    // Copying takes the text to both.
    host.dispatch(Action::Copy);
    assert_eq!(host.copied, [line[..4].to_owned()]);
    assert_eq!(host.primary, [line[..4].to_owned()]);
    assert_eq!(host.model().status, "Copied 1 line.");

    // A click without a drag leaves an empty selection, which reaches neither clipboard.
    host.update(|model| model.press_text(text_press(row, 2)));
    host.update(Model::release_text);
    host.dispatch(Action::Copy);
    assert_eq!(host.copied.len(), 1);
}

/// With `[tui.mouse] copy_on_select`, ending a drag copies what it selected, as `y` does.
#[test]
fn copy_on_select_copies_when_a_drag_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = settings();
    settings.mouse.copy_on_select = true;
    let mut host = Host::open_with(demo(dir.path()), settings);
    let (row, line) = engine_line(&mut host);
    host.update(|model| model.press_text(text_press(row, 0)));
    host.update(|model| model.drag_text(Pane::Diff, TextPos { row, col: 4 }));
    host.update(Model::release_text);
    assert_eq!(host.copied, [line[..4].to_owned()]);
    assert_eq!(host.primary, [line[..4].to_owned()]);
    assert_eq!(host.model().status, "Copied 1 line.");
}

/// With `[tui.mouse] navigate` off, a press in the diff still selects text but neither selects its
/// hunk nor opens a gap, and a click in the files moves nothing; the keys still do.
#[test]
fn without_navigate_a_press_only_selects() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = settings();
    settings.mouse.navigate = false;
    let mut host = Host::open_with(demo(dir.path()), settings);
    let (row, line) = engine_line(&mut host);
    host.dispatch(Action::FirstHunk);
    let hunk = host.model().hunk;
    let other = (0..host.model().rows.rows.len())
        .rev()
        .find(|&other| {
            host.model().rows.rows[other]
                .hunk
                .is_some_and(|found| found != hunk)
        })
        .unwrap();
    host.update(|model| model.press_text(text_press(other, 0)));
    assert_eq!(host.model().hunk, hunk, "the press selected no hunk");
    host.update(|model| model.press_text(text_press(row, 0)));
    host.update(|model| model.drag_text(Pane::Diff, TextPos { row, col: 4 }));
    host.update(Model::release_text);
    host.dispatch(Action::Copy);
    assert_eq!(host.copied, [line[..4].to_owned()]);

    host.dispatch(Action::FocusTree);
    host.dispatch(Action::FocusTree);
    let cursor = host.model().tree_cursor;
    host.update(|model| model.click_tree_row(cursor + 1, cursor + 1, 1));
    assert_eq!(host.model().tree_cursor, cursor, "the click moved nothing");
    host.dispatch(Action::NextFile);
    assert_ne!(host.model().tree_cursor, cursor, "the keys still move");
}

/// With `[tui.mouse] select` off, a press starts no selection, so a drag selects nothing and a
/// right click has nothing to copy, while the press still selects the hunk under it.
#[test]
fn without_select_a_press_only_navigates() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = settings();
    settings.mouse.select = false;
    let mut host = Host::open_with(demo(dir.path()), settings);
    engine_line(&mut host);
    host.dispatch(Action::FirstHunk);
    let first = host.model().hunk;
    let (row, hunk) = (0..host.model().rows.rows.len())
        .rev()
        .find_map(|row| {
            let hunk = host.model().rows.rows[row].hunk?;
            (hunk != first).then_some((row, hunk))
        })
        .unwrap();
    host.update(|model| model.press_text(text_press(row, 0)));
    host.update(|model| model.drag_text(Pane::Diff, TextPos { row, col: 4 }));
    host.update(Model::release_text);
    assert_eq!(host.model().selection, None);
    assert_eq!(host.model().hunk, hunk);
}

/// With `[tui.mouse] scroll` off, the wheel and a sideways turn move nothing, while the scroll and
/// pan keys still do.
#[test]
fn without_scroll_the_wheel_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = whole_file();
    settings.mouse.scroll = false;
    let mut host = Host::open_with(demo(dir.path()), settings);
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let top = |host: &Host| host.model().diff_scroll.offset();
    let before = top(&host);
    assert!(before > 0.0, "the file opens scrolled to its first change");
    host.update(|model| model.wheel_list(List::Diff, Wheel::Precise(-5.0)));
    host.update(|model| model.pan_text(4.0));
    assert_eq!(top(&host), before);
    assert_eq!(host.model().diff_text().offset(), 0.0);
    host.dispatch(Action::HalfPageUp);
    lay_out(&mut host);
    settle_scroll(&mut host);
    assert_ne!(top(&host), before, "the keys still scroll");
    host.dispatch(Action::PanRight);
    assert_ne!(host.model().diff_text().offset(), 0.0, "the keys still pan");
}

/// Shows `src/engine.rs` in the diff, and returns the first code row longer than four bytes
/// and its text.
fn engine_line(host: &mut Host) -> (usize, String) {
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    let model = host.model();
    (0..model.rows.rows.len())
        .find_map(|row| {
            let code = model.copyable(Pane::Diff, Column::Whole, row)?;
            (model.rows.rows[row].is_code() && code.len() > 4).then(|| (row, code.to_owned()))
        })
        .unwrap()
}

/// A single press in the diff's whole column at `row` and `col`, on rows that do not wrap,
/// so each row is its own screen row.
fn text_press(row: usize, col: usize) -> TextPress {
    TextPress {
        pane: Pane::Diff,
        column: Column::Whole,
        pos: TextPos { row, col },
        screen: row,
        clicks: 1,
        extend: false,
    }
}

/// Runs a frame's layout phase in the fixed window, as a front end does before it draws.
fn lay_out(host: &mut Host) {
    host.update(|model| model.layout(fixed_window(), Instant::now()));
}

/// A note wider than the diff, such as a symlink's entry before a change, can be panned to its
/// end even where no code is that wide, in either layout.
#[test]
fn the_pan_reaches_the_end_of_every_note() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("docs/current"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let model = host.model();
    let (_, widest) = model.rows.widest_annotation.unwrap();
    let overflow = scroll::rows(widest) - CODE_ROOM;
    assert!(overflow > 0.0, "the symlink's entry is wider than the diff");
    assert!(
        model
            .rows
            .widest_code
            .is_none_or(|(_, cells)| scroll::rows(cells) < CODE_ROOM)
    );
    assert!(model.diff_text().extent() >= overflow);

    host.dispatch(Action::ToggleLayout);
    lay_out(&mut host);
    assert!(host.model().side_by_side());
    assert!(host.model().diff_text().extent() >= overflow);
}

/// Every one of the tool's own rows moves with the code only as far as it runs past its room,
/// with its clip marker while some of it is cut, and the pan's end shows the end of each.
#[test]
fn notes_move_with_the_code_until_their_end_shows() {
    use crate::paint::annotation::{NotePan, note_pan};

    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("docs/current"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let pans = |model: &Model| -> Vec<(usize, NotePan)> {
        model
            .rows
            .rows
            .iter()
            .filter_map(|row| match &row.body {
                rows::Body::Note(note) => {
                    let cells = note.width();
                    Some((cells, note_pan(model, scroll::float(cells))))
                }
                _ => None,
            })
            .collect()
    };

    // Unpanned, nothing moves, and the symlink's entry, wider than the diff, is cut.
    let unpanned = pans(host.model());
    assert!(unpanned.iter().all(|(_, pan)| pan.shift == 0.0));
    let (widest, _) = host.model().rows.widest_annotation.unwrap();
    let Some(rows::Body::Note(note)) = host.model().rows.rows.get(widest).map(|row| &row.body)
    else {
        panic!("the widest annotation is a note");
    };
    assert!(note_pan(host.model(), scroll::float(note.width())).cut);

    // Part of the way, a row moves as far as the code or as far as it runs past, whichever is
    // less, and stays marked until then.
    host.update(|model| model.pan_text(2.0));
    for (cells, pan) in pans(host.model()) {
        let overflow = (scroll::rows(cells) - CODE_ROOM).max(0.0);
        assert_eq!(pan.shift, overflow.min(2.0), "{cells} cells");
        assert_eq!(pan.cut, pan.shift < overflow, "{cells} cells");
    }

    // At the pan's end, every row shows whole.
    host.update(|model| model.pan_text(f64::MAX));
    assert!(
        pans(host.model()).iter().all(|(_, pan)| !pan.cut),
        "{:?}",
        pans(host.model())
    );
}

/// A row moves with the code by what it draws, not by the upper bound the pan's extent comes
/// from: one that fits, however close to its room's edge, never moves or shows the marker,
/// and one that is cut moves exactly as far as it really runs past.
#[test]
fn notes_move_by_what_they_draw() {
    use crate::paint::annotation::{NotePan, note_pan};

    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("docs/current"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    // The symlink's entry lets the code pan well past a few cells.
    host.update(|model| model.pan_text(f64::MAX));
    assert!(host.model().diff_text().offset() > 3.0);
    let room = scroll::narrow(CODE_ROOM);
    let pan = |reach: f32| note_pan(host.model(), reach);

    let unmoved = NotePan {
        shift: 0.0,
        cut: false,
    };
    assert_eq!(pan(room), unmoved, "a row that just fits");
    assert_eq!(pan(room - 0.5), unmoved, "a row within a cell of its edge");
    assert_eq!(
        pan(room + 3.0),
        NotePan {
            shift: 3.0,
            cut: false
        },
        "a cut row moves by its overflow and no further"
    );

    // Panned part of the way, a cut row moves with the code and stays marked.
    host.update(|model| model.set_text_offset(1.0));
    assert_eq!(
        note_pan(host.model(), room + 3.0),
        NotePan {
            shift: 1.0,
            cut: true
        }
    );
}

/// The first row of `layout` whose line wraps, with its screen rows.
fn wrapped_row(layout: &rows::Layout) -> (usize, std::ops::Range<usize>) {
    (0..layout.rows.len())
        .map(|row| (row, layout.screen_rows(row)))
        .find(|(_, screen)| screen.len() > 1)
        .unwrap()
}

/// `w` wraps long lines onto screen rows of their own, which nothing pans past, while the rows
/// stay: a drag across a break selects and copies one line, and a press past a short piece's
/// end lands at that end, not in the next piece. `w` again cuts the lines as before.
#[test]
fn long_lines_wrap_and_still_copy_as_lines() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let rows = host.model().rows.rows.len();
    host.dispatch(Action::ToggleWrap);
    lay_out(&mut host);
    let model = host.model();
    assert!(model.look().wrap);
    assert!(model.rows.screen.len() > rows, "long lines take more rows");
    assert_eq!(model.rows.rows.len(), rows, "the rows stay");
    for screen in 0..model.rows.screen.len() {
        let shown = model.diff_shown(screen, Column::Whole).unwrap_or_default();
        assert!(
            crate::paint::display::display_width(shown) <= 40,
            "{shown:?} fits its room"
        );
    }
    let wrapped_extent = model.unified_text.extent();

    // Only the tool's own rows still pan; the code stays where it wraps.
    host.dispatch(Action::PanRight);
    assert_eq!(host.model().code_offset(), 0.0);
    let model = host.model();
    let (row, screen) = wrapped_row(&model.rows);
    let rows::Body::Code(cell) = &model.rows.rows[row].body else {
        panic!("a wrapped line across the width");
    };
    let code = cell.code().to_owned();
    let second = model.rows.screen[screen.start + 1].shown[0]
        .clone()
        .unwrap();
    let start = model.diff_pos(screen.start, Column::Whole, Hit::Byte(0));
    let end = model.diff_pos(screen.start + 1, Column::Whole, Hit::End);
    assert_eq!(start, Some(TextPos { row, col: 0 }));
    assert_eq!(
        model.diff_pos(screen.start, Column::Whole, Hit::Byte(1000)),
        Some(TextPos {
            row,
            col: second.start
        }),
        "past the first piece's end, at its end"
    );
    host.update(|model| {
        model.press_text(TextPress {
            screen: screen.start,
            ..text_press(row, 0)
        });
        model.drag_text(Pane::Diff, end.unwrap());
    });
    assert_eq!(
        host.model().selected_text().as_deref(),
        code.get(..second.end.min(code.len())),
        "one line, without a break where it wraps"
    );

    host.dispatch(Action::ToggleWrap);
    lay_out(&mut host);
    let model = host.model();
    assert_eq!(model.rows.screen.len(), rows);
    assert_eq!(model.rows.wrapped, None);
    assert!(
        model.unified_text.extent() > wrapped_extent,
        "long lines pan again"
    );
}

/// A change of the diff's look keeps the piece of a wrapped line at the top, not only the line:
/// a style changes no row, and a larger code size wraps the line again around the same byte.
#[test]
fn a_look_change_keeps_the_wrapped_piece_at_the_top() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleWrap);
    lay_out(&mut host);
    let (row, screen) = wrapped_row(&host.model().rows);
    host.update(|model| model.drag_list_to(List::Diff, scroll::rows(screen.start + 1)));
    // The byte of the line at the top, and whether the top shows it.
    let byte = host.model().rows.screen[screen.start + 1].shown[0]
        .clone()
        .unwrap()
        .start;
    let shows = |model: &Model| {
        let top = scroll::whole_rows(model.diff_scroll.offset());
        let piece = &model.rows.screen[top];
        piece.row == row && piece.shown[0].clone().unwrap().contains(&byte)
    };
    host.dispatch(Action::ToggleDiffStyle);
    assert!(shows(host.model()), "a style keeps it");
    host.dispatch(Action::ZoomIn);
    lay_out(&mut host);
    assert!(shows(host.model()), "a larger code size keeps it");
}

/// Side by side, a side whose line ended on an earlier screen row shows its background alone:
/// no number, no sign and no change bar beside the other side's wrapped line.
#[test]
fn a_finished_side_shows_no_change_bar() {
    use crate::paint::Palette;
    use crate::paint::code::{At, code_cell};

    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| {
        model.settings.appearance.applied.style.change_bar = true;
        model.show_file("src/engine.rs");
    });
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleLayout);
    host.dispatch(Action::ToggleWrap);
    let narrow = Geometry {
        diff_half: 20.0,
        ..fixed_window()
    };
    host.update(|model| model.layout(narrow, Instant::now()));
    let model = host.model();
    let palette = Palette::of(model);
    // A screen row with a line on its left whose piece ended on an earlier screen row.
    let (screen, cell, file) = (0..model.rows.screen.len())
        .find_map(|screen| {
            let piece = &model.rows.screen[screen];
            let row = &model.rows.rows[piece.row];
            let cell = row.sides()[0].filter(|_| piece.shown[0].is_none())?;
            cell.kind
                .ne(&rows::RowKind::Context)
                .then_some((screen, cell, row.file))
        })
        .unwrap();
    let at = At {
        row: screen,
        file,
        column: Column::Left,
    };
    let painted = code_cell(model, &palette, at, cell);
    assert_eq!(painted.bar, None);
    assert_eq!(painted.number.text, "");
    assert_eq!(painted.shown, cell.text.len()..cell.text.len());
}

/// Read partway into a wrapped added line, a switch to side by side shows that line from its
/// start in its half, never the other half's line where the byte would fall.
#[test]
fn another_layout_shows_the_wrapped_line_from_its_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleWrap);
    // Halves narrow enough that the base's line wraps too, and a diff short enough that the
    // line can scroll to its top.
    let narrow = Geometry {
        diff_half: 12.0,
        heights: Heights {
            diff: Some(6.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    host.update(|model| model.layout(narrow, Instant::now()));
    let model = host.model();
    let (row, screen) = (0..model.rows.rows.len())
        .map(|row| (row, model.rows.screen_rows(row)))
        .find(|&(row, ref screen)| {
            screen.len() > 1 && model.rows.rows[row].kind() == rows::RowKind::Added
        })
        .unwrap();
    let line = model.rows.rows[row].sides()[0]
        .and_then(|cell| cell.head.clone())
        .unwrap()
        .line;
    host.update(|model| model.drag_list_to(List::Diff, scroll::rows(screen.start + 1)));
    host.dispatch(Action::ToggleLayout);
    host.update(|model| model.layout(narrow, Instant::now()));
    let model = host.model();
    assert!(model.side_by_side());
    let top = scroll::whole_rows(model.diff_scroll.offset());
    let piece = &model.rows.screen[top];
    let head = model.rows.rows[piece.row].sides()[1]
        .and_then(|cell| cell.head.clone())
        .map(|head| head.line);
    assert_eq!(head, Some(line), "the same line at the top");
    assert_eq!(
        piece.shown[1].clone().map(|shown| shown.start),
        Some(0),
        "from its start"
    );
}

/// A file's remembered place is its row alone: back from another file, after the layout changed
/// meanwhile, a wrapped line at the top shows from its start, never at a byte of other text.
#[test]
fn a_remembered_place_shows_the_wrapped_line_from_its_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleWrap);
    let short = Geometry {
        diff_half: 12.0,
        heights: Heights {
            diff: Some(6.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    host.update(|model| model.layout(short, Instant::now()));
    let model = host.model();
    let (row, screen) = (0..model.rows.rows.len())
        .map(|row| (row, model.rows.screen_rows(row)))
        .find(|&(row, ref screen)| {
            screen.len() > 1 && model.rows.rows[row].kind() == rows::RowKind::Added
        })
        .unwrap();
    let line = model.rows.rows[row].sides()[0]
        .and_then(|cell| cell.head.clone())
        .unwrap()
        .line;
    host.update(|model| model.drag_list_to(List::Diff, scroll::rows(screen.start + 1)));
    host.update(|model| model.show_file("src/format.rs"));
    host.dispatch(Action::ToggleLayout);
    host.update(|model| model.layout(short, Instant::now()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.update(|model| model.layout(short, Instant::now()));
    let model = host.model();
    let top = scroll::whole_rows(model.diff_scroll.offset());
    let piece = &model.rows.screen[top];
    let head = model.rows.rows[piece.row].sides()[1]
        .and_then(|cell| cell.head.clone())
        .map(|head| head.line);
    assert_eq!(head, Some(line), "the same line at the top");
    assert!(model.rows.starts_row(top), "from its start");
}

/// A wrapped line's piece carries over on the same rows, and to other rows only on the very same
/// line: the same text on another line, in another file, or on a line without a number shows
/// from its start.
#[test]
fn a_piece_carries_over_only_to_the_same_line() {
    use super::present::same_start;

    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::ToggleWrap);
    lay_out(&mut host);
    let old = host.model().rows.as_ref().clone();
    let (row, screen) = wrapped_row(&old);
    let second = screen.start + 1;
    let source = old.rows[row].source;
    let start = old.screen[second].start();
    assert_eq!(
        same_start(&old, second, &old, source),
        start,
        "the same line"
    );

    let mut other_line = old.clone();
    if let rows::Body::Code(cell) = &mut other_line.rows[row].body {
        cell.head = cell.head.clone().map(|head| rows::HeadLine {
            line: head.line + 1,
            ..head
        });
        cell.base = cell.base.map(|base| base + 1);
    }
    assert_eq!(same_start(&old, second, &other_line, source), None);

    let mut other_file = old.clone();
    other_file.files[old.rows[row].file].path = "src/other.rs".to_owned();
    assert_eq!(same_start(&old, second, &other_file, source), None);

    let mut unnumbered = old.clone();
    if let rows::Body::Code(cell) = &mut unnumbered.rows[row].body {
        cell.head = None;
        cell.base = None;
    }
    assert_eq!(
        same_start(&unnumbered, second, &unnumbered, source),
        start,
        "the same rows"
    );
    assert_eq!(
        same_start(&unnumbered, second, &unnumbered.clone(), source),
        None,
        "other rows"
    );
}

/// `w` lays the rows out on screen again where they are, never copying them.
#[test]
fn wrapping_changes_the_rows_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    lay_out(&mut host);
    let rows = Arc::as_ptr(&host.model().rows);
    host.dispatch(Action::ToggleWrap);
    assert!(host.model().rows.wrapped.is_some());
    assert!(std::ptr::eq(Arc::as_ptr(&host.model().rows), rows));
}

/// A row draws its line-ending marker after its code, which a press maps to like the code but
/// never selects: past the code, it lands at the code's end.
#[test]
fn a_press_on_a_line_ending_lands_at_the_code_end() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    let text = "let crlf = 1;␍";
    let code = "let crlf = 1;".len();
    host.update(|model| {
        let mut layout = rows::Layout {
            rows: vec![rows::Row {
                file: 0,
                unit: "u1-0".parse().unwrap(),
                hunk: None,
                source: 0,
                body: rows::Body::Code(rows::Cell {
                    kind: rows::RowKind::Context,
                    text: text.to_owned(),
                    content_len: code,
                    highlights: Vec::new(),
                    deleted: Vec::new(),
                    head: None,
                    base: None,
                    quiet: false,
                    moved: false,
                    reviewed: None,
                }),
            }],
            ..rows::Layout::default()
        };
        rows::project(&mut layout, None);
        model.rows = Arc::new(layout);
    });
    let model = host.model();
    assert_eq!(model.diff_shown(0, Column::Whole), Some(text));
    let at = |byte| model.diff_pos(0, Column::Whole, Hit::Byte(byte));
    assert_eq!(at(4), Some(TextPos { row: 0, col: 4 }));
    assert_eq!(at(code + 1), Some(TextPos { row: 0, col: code }));
}

/// Finding text on a later piece of a wrapped line scrolls to the screen row that shows it,
/// not only to the line's first.
#[test]
fn a_match_on_a_wrapped_piece_shows() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleWrap);
    // Narrow enough that the line's first screen row and its last are far apart.
    let narrow = Geometry {
        diff_list: 12.0,
        ..fixed_window()
    };
    host.update(|model| model.layout(narrow, Instant::now()));
    // The last piece of the last line that wraps, which is far down the file.
    let model = host.model();
    let (row, screen) = (0..model.rows.rows.len())
        .rev()
        .map(|row| (row, model.rows.screen_rows(row)))
        .find(|(_, screen)| screen.len() > 6)
        .unwrap();
    let last = screen.end - 1;
    let query = model
        .diff_shown(last, Column::Whole)
        .unwrap()
        .trim()
        .to_owned();
    host.update(|model| model.drag_list_to(List::Diff, 0.0));
    host.dispatch(Action::OpenFind);
    host.update(|model| model.edit_field(Field::Find, &query));
    host.update(|model| model.submit_field(Field::Find));
    let model = host.model();
    let find = model.find.as_ref().unwrap();
    let found = find.current.and_then(|current| find.matches.get(current));
    assert_eq!(found.map(|found| found.row), Some(row), "{query:?}");
    let top = model.diff_scroll.target();
    let shows = scroll::rows(last) >= top && scroll::rows(last) + 1.0 <= top + 40.0;
    assert!(shows, "screen row {last} shows from {top}");
}

/// A match several screen rows tall shows whole when the diff is tall enough for it, even when
/// the margins a match keeps around it no longer fit.
#[test]
fn a_tall_match_shows_whole() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleWrap);
    let short = Geometry {
        diff_list: 12.0,
        heights: Heights {
            diff: Some(12.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    host.update(|model| model.layout(short, Instant::now()));
    // Seven screen rows of the last line without deleted text that wraps onto eight or more.
    let model = host.model();
    let (row, screen) = (0..model.rows.rows.len())
        .rev()
        .filter(|&row| {
            matches!(&model.rows.rows[row].body,
                rows::Body::Code(cell) if cell.deleted.is_empty())
        })
        .map(|row| (row, model.rows.screen_rows(row)))
        .find(|(_, screen)| screen.len() >= 8)
        .unwrap();
    let piece = |index: usize| model.rows.screen[index].shown[0].clone().unwrap();
    let rows::Body::Code(cell) = &model.rows.rows[row].body else {
        panic!("a wrapped line across the width");
    };
    let query = cell.text[piece(screen.start + 1).start..piece(screen.start + 7).end].to_owned();
    let (first, last) = (screen.start + 1, screen.start + 7);
    host.update(|model| model.drag_list_to(List::Diff, 0.0));
    host.dispatch(Action::OpenFind);
    host.update(|model| model.edit_field(Field::Find, &query));
    host.update(|model| model.submit_field(Field::Find));
    let top = host.model().diff_scroll.target();
    let shows =
        |screen: usize| scroll::rows(screen) >= top && scroll::rows(screen) + 1.0 <= top + 12.0;
    assert!(
        shows(first) && shows(last),
        "screen rows {first} to {last} show from {top}"
    );
}

/// A narrower diff wraps its lines again without laying them out anew: the selection still
/// names the same text, and the text at the top stays at the top.
#[test]
fn a_resize_rewraps_and_keeps_the_selection_and_the_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleWrap);
    lay_out(&mut host);
    let (row, screen) = wrapped_row(&host.model().rows);
    // The second piece of the line at the top, and a word of the line selected.
    host.update(|model| {
        model.drag_list_to(List::Diff, scroll::rows(screen.start + 1));
        model.press_text(TextPress {
            clicks: 2,
            screen: screen.start,
            ..text_press(row, 0)
        });
    });
    let top = |model: &Model| {
        let screen = scroll::whole_rows(model.diff_scroll.offset());
        let piece = &model.rows.screen[screen];
        (piece.row, piece.shown[0].clone().unwrap())
    };
    let (_, before) = top(host.model());
    let selected = host.model().selected_text();
    assert!(selected.is_some());

    let narrower = Geometry {
        diff_list: 30.0,
        ..fixed_window()
    };
    host.update(|model| model.layout(narrower, Instant::now()));
    let model = host.model();
    assert_eq!(model.rows.wrapped.map(|room| room.whole), Some(30));
    assert_eq!(model.selected_text(), selected, "the same text selected");
    let (at, piece) = top(model);
    assert_eq!(at, row, "the same line at the top");
    assert!(
        piece.start <= before.start && before.start < piece.end,
        "the piece at the top holds the byte that was there: {piece:?}, {before:?}"
    );
}

/// The pan keys move the unified diff's code half its room at a time, as far as its widest
/// line reaches and back; the side-by-side halves keep their own offset, and another file
/// starts at its left edge.
#[test]
fn the_pan_keys_move_code_half_its_room() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let (_, widest) = host.model().rows.widest_code.unwrap();
    let extent = crate::frame::pan_extent(widest, CODE_ROOM);
    // The fixed window's room of 40 cells takes the file's widest line well past its end.
    assert!(extent > CODE_ROOM, "{extent}");
    let offset = |host: &Host| host.model().unified_text.offset();

    host.dispatch(Action::PanRight);
    assert_eq!(offset(&host), 20.0);
    host.dispatch(Action::PanRight);
    host.dispatch(Action::PanRight);
    assert_eq!(
        offset(&host),
        extent,
        "no further than the widest line reaches"
    );
    host.dispatch(Action::PanLeft);
    assert_eq!(offset(&host), extent - 20.0);
    assert_eq!(
        host.model().text.offset(),
        0.0,
        "the halves keep their own offset"
    );

    host.update(|model| model.show_file("src/parser.rs"));
    assert_eq!(offset(&host), 0.0, "another file starts at its left edge");
}

/// The buffer's code follows its cursor sideways, keeping cells beside it, and the pan keys
/// move it half its room as in the diff.
#[test]
fn the_buffer_follows_its_cursor_sideways() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleBuffer);
    let row = host
        .model()
        .buffer
        .as_ref()
        .unwrap()
        .lines
        .lines
        .iter()
        .position(|line| line.contains("usize::MAX"))
        .unwrap();
    let column = host
        .model()
        .buffer
        .as_ref()
        .unwrap()
        .line(row)
        .find("MAX")
        .unwrap();
    let offset = |host: &Host| host.model().buffer_text.offset();

    // The cursor past the room's end: the code moves so it shows with four cells after it.
    host.update(|model| model.set_buffer_cursor(row, column));
    assert_eq!(offset(&host), scroll::rows(column) + 1.0 + 4.0 - CODE_ROOM);
    // Back at the start of the file, the code is back at its left edge.
    host.dispatch(Action::BufferTop);
    assert_eq!(offset(&host), 0.0);
    host.dispatch(Action::PanRight);
    assert_eq!(offset(&host), 20.0);
}

/// Before the buffer list's first layout its room is unknown, so the cursor is revealed
/// sideways by the first layout phase that has the list's width, never against no room, which
/// would move the cursor and the start of every line out of view, and centered by the first
/// that has its height.
#[test]
fn the_buffer_waits_for_its_width_to_follow_its_cursor_sideways() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    // The buffer list has not been laid out, as in a window before its first buffer shows.
    let window = fixed_window();
    let unmeasured = Geometry {
        buffer_list: 0.0,
        heights: Heights {
            buffer: None,
            ..window.heights
        },
        ..window
    };
    host.update(|model| model.layout(unmeasured, Instant::now()));
    host.dispatch(Action::ToggleBuffer);
    let buffer = host.model().buffer.as_ref().unwrap();
    let row = buffer
        .lines
        .lines
        .iter()
        .position(|line| line.contains("usize::MAX"))
        .unwrap();
    let column = buffer.line(row).find("MAX").unwrap();
    let offset = |host: &Host| host.model().buffer_text.offset();

    // A cursor at a line's start, as a search match can put it, keeps that start in view.
    host.update(|model| model.set_buffer_cursor(row, 0));
    assert_eq!(offset(&host), 0.0);
    // A cursor past where the room will end waits for the room.
    host.update(|model| model.set_buffer_cursor(row, column));
    assert_eq!(offset(&host), 0.0);

    // The first layout with the list's width shows the cursor with four cells after it, and
    // with its height centers the cursor's line.
    host.update(|model| model.layout(window, Instant::now()));
    assert_eq!(offset(&host), scroll::rows(column) + 1.0 + 4.0 - CODE_ROOM);
    let rows = host.model().list_rows(List::Buffer);
    assert_eq!(
        host.model().buffer_scroll.offset(),
        (scroll::rows(row) + 0.5 - 20.0).clamp(0.0, scroll::rows(rows) - 40.0)
    );
    // Later layouts leave the reviewer's own pan alone.
    host.update(|model| model.pan_buffer(-3.0));
    let panned = offset(&host);
    host.update(|model| model.layout(window, Instant::now()));
    assert_eq!(offset(&host), panned);
}

/// A buffer cursor that showed keeps showing when its room narrows under it, as when the files
/// pane widens to fit a file that just appeared: the code moves to show it again. A cursor
/// the reviewer panned away from is left where it is.
#[test]
fn the_buffer_cursor_keeps_in_view_when_its_room_narrows() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.dispatch(Action::ToggleBuffer);
    let buffer = host.model().buffer.as_ref().unwrap();
    let row = buffer
        .lines
        .lines
        .iter()
        .position(|line| line.contains("usize::MAX"))
        .unwrap();
    let column = buffer.line(row).find("MAX").unwrap();
    let offset = |host: &Host| host.model().buffer_text.offset();
    // Room for the cursor with twenty cells to spare, after the four digits of the numbers.
    let window = fixed_window();
    let wide = Geometry {
        buffer_list: scroll::float(column) + 24.0,
        ..window
    };
    host.update(|model| model.layout(wide, Instant::now()));
    host.update(|model| model.set_buffer_cursor(row, column));
    assert_eq!(offset(&host), 0.0, "the cursor shows at the left edge");

    // The room narrows to forty cells, which end before the cursor.
    host.update(|model| model.layout(window, Instant::now()));
    assert_eq!(offset(&host), scroll::rows(column) + 1.0 + 4.0 - CODE_ROOM);

    // A cursor the reviewer panned out of view stays out of view when the room changes.
    host.update(|model| model.set_buffer_cursor(row, 0));
    assert_eq!(offset(&host), 0.0);
    host.update(|model| model.pan_buffer(3.0));
    assert_eq!(offset(&host), 3.0);
    let narrower = Geometry {
        buffer_list: window.buffer_list - 2.0,
        ..window
    };
    host.update(|model| model.layout(narrower, Instant::now()));
    assert_eq!(offset(&host), 3.0, "the reviewer's pan is kept");
}

/// A fitted files pane follows the tree's widest row, every folder open: a nested file that
/// starts changing in a live review widens it, folding never moves it, and the file's change
/// going away narrows it again.
#[test]
fn the_tree_s_width_follows_the_files_that_change() {
    let dir = tempfile::tempdir().unwrap();
    let nested = "deep/nested/long_file_name.rs";
    std::fs::create_dir_all(dir.path().join("repo/deep/nested")).unwrap();
    let (review, repo) = worktree_review_of(
        dir.path(),
        &[("a.rs", "fn a() {}\n"), (nested, "fn b() {}\n")],
        &[("a.rs", "fn a() { 1 }\n")],
    );
    let mut host = Host::open(review);
    // `a.rs` at the top: its marker and name.
    assert_eq!(host.model().tree_cells(), 2 + 4);

    std::fs::write(repo.join(nested), "fn b() { 2 }\n").unwrap();
    host.update(|model| model.capture(false));
    host.settle();
    // `long_file_name.rs` under the chain `deep/nested`: an indent, its marker and its name.
    assert_eq!(host.model().tree_cells(), 2 + 2 + 17);

    host.update(|model| model.tree_cursor = 0);
    host.dispatch(Action::FocusTree);
    host.dispatch(Action::TreeCollapse);
    assert!(host.model().tree_collapsed.contains("deep/nested"));
    assert_eq!(host.model().tree_cells(), 2 + 2 + 17, "a fold keeps it");

    std::fs::write(repo.join(nested), "fn b() {}\n").unwrap();
    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(host.model().tree_cells(), 2 + 4);
}

/// A folded folder that a chain takes in keeps the chain folded; unfolding the chain unfolds
/// every folder of it, so none folds again when the chain parts.
#[test]
fn a_chain_folds_with_its_folders_and_unfolds_them_all() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("repo/a/b")).unwrap();
    let (review, repo) = worktree_review_of(
        dir.path(),
        &[("a/side.rs", "fn s() {}\n"), ("a/b/f.rs", "fn f() {}\n")],
        &[
            ("a/side.rs", "fn s() { 1 }\n"),
            ("a/b/f.rs", "fn f() { 1 }\n"),
        ],
    );
    let mut host = Host::open(review);
    let paths = |model: &Model| -> Vec<String> {
        model
            .visible_tree()
            .iter()
            .map(|&index| model.tree[index].path.clone())
            .collect()
    };
    host.update(|model| model.tree_cursor = 0);
    host.dispatch(Action::FocusTree);
    host.dispatch(Action::TreeCollapse);
    assert_eq!(paths(host.model()), ["a"]);

    // `a/side.rs` changes no more, so `a` holds only `a/b`: one chain, still folded.
    std::fs::write(repo.join("a/side.rs"), "fn s() {}\n").unwrap();
    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(paths(host.model()), ["a/b"]);
    assert_eq!(host.model().tree[0].name, "a/b");

    host.update(|model| model.tree_cursor = 0);
    host.update(|model| {
        model.tree_fold(Some(false));
    });
    assert_eq!(paths(host.model()), ["a/b", "a/b/f.rs"]);
    assert!(
        host.model().tree_collapsed.is_empty(),
        "both folders unfold"
    );

    // `a/side.rs` changes again, parting the chain: nothing folds.
    std::fs::write(repo.join("a/side.rs"), "fn s() { 2 }\n").unwrap();
    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(paths(host.model()), ["a", "a/b", "a/b/f.rs", "a/side.rs"]);
}

/// Dragging the divider sets the files pane's width, which is remembered as a number, and a
/// double click fits the tree again, which is remembered as `"fit"`.
#[test]
fn a_dragged_width_and_fitting_again_are_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.settings.config_path = Some(config.clone()));
    let remembered = || {
        crate::config::parse(&std::fs::read_to_string(&config).unwrap())
            .unwrap()
            .layout
            .sidebar
    };

    host.update(|model| model.set_sidebar_width(420.0));
    host.update(Model::remember_layout);
    assert_eq!(
        host.model().layout.sidebar,
        crate::panes::SidebarWidth::Set(420.0)
    );
    host.settle();
    assert_eq!(remembered(), crate::panes::SidebarWidth::Set(420.0));

    host.update(Model::reset_sidebar_width);
    assert_eq!(host.model().layout.sidebar, crate::panes::SidebarWidth::Fit);
    host.settle();
    assert_eq!(remembered(), crate::panes::SidebarWidth::Fit);
    assert!(
        std::fs::read_to_string(&config)
            .unwrap()
            .contains("sidebar = \"fit\"")
    );
}

/// Dragging the comparison's edge sets its height, which is remembered as a number in
/// `[layout]`, keeping the rest of the file: its comments, the files pane's width and
/// `[tui.mouse]`.
#[test]
fn a_dragged_comparison_height_is_remembered_keeping_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        indoc! {"
            # Mine.
            [layout]
            sidebar = 320 # dragged
            [tui.mouse]
            navigate = false
        "},
    )
    .unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.settings.config_path = Some(config.clone()));

    host.update(|model| model.remember_comparison_height(512.4));
    assert_eq!(host.model().layout.comparison, Some(512.4));
    host.settle();
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        indoc! {"
            # Mine.
            [layout]
            sidebar = 320 # dragged
            comparison = 512
            [tui.mouse]
            navigate = false
        "}
    );
}

/// Settings changed in the app apply at once and are written to the configuration file in the
/// background, one write at a time in the order made, so edits of different keys all land.
#[test]
fn settings_are_remembered_in_order_one_write_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.settings.config_path = Some(config.clone()));
    let text = || std::fs::read_to_string(&config).unwrap_or_default();

    // Two edits of different keys, one after the other: both apply at once, and only the first
    // is being written.
    host.update(|model| {
        model.set_sidebar_width(420.0);
        model.remember_layout();
    });
    host.update(|model| model.apply_wrap(true));
    assert!(host.model().look().wrap, "the setting applies at once");
    assert_eq!(host.jobs.len(), 1, "one write runs at a time");
    assert_eq!(text(), "", "nothing is written on the UI thread");

    // The first write lands the pane width alone, and starts the second.
    let first = host.run_job();
    assert!(text().contains("sidebar = 420"));
    assert!(!text().contains("wrap"));
    host.complete(first);
    assert_eq!(host.jobs.len(), 1, "the next write starts");

    // The second adds the wrap, keeping the width.
    host.settle();
    let remembered = crate::config::parse(&text()).unwrap();
    assert_eq!(
        remembered.layout.sidebar,
        crate::panes::SidebarWidth::Set(420.0)
    );
    assert!(remembered.wrap);
    assert!(!host.model().busy());
}

/// A write of the configuration that fails says so on the status line once it ends, and the
/// log keeps it.
#[test]
fn a_failed_write_of_the_configuration_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    // Not TOML, which a write would lose.
    std::fs::write(&config, "not = [toml").unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.settings.config_path = Some(config.clone()));

    host.update(|model| model.apply_wrap(true));
    assert_eq!(host.model().status, "Long lines wrap.");
    host.settle();
    assert!(
        host.model()
            .status
            .starts_with("Long lines wrap applied, could not save: cannot edit"),
        "{}",
        host.model().status
    );
    assert!(
        host.logged
            .iter()
            .any(|line| line.starts_with("could not save the configuration")),
        "{:?}",
        host.logged
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "not = [toml");
}

/// Once quitting is authorized the driver takes no more input: a key right after the quit, as
/// a key sequence hands on after replaying it, a divider's release and a folded folder change
/// nothing and save nothing.
#[test]
fn no_input_lands_once_quitting_is_authorized() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    // A first session saves the place, so the next quit has nothing left to save.
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::Quit);
    host.settle();
    let review = prepare(
        &DemoPaths::under(dir.path()),
        &fixture_dir(),
        DemoRequest::UpTo(RevisionNumber::new(1).unwrap()),
    )
    .unwrap();
    let mut host = Host::open(review);
    host.update(|model| model.settings.config_path = Some(config.clone()));
    host.dispatch(Action::Quit);
    assert_eq!(
        host.model().status,
        "Closing the review before quitting…",
        "nothing is left to save"
    );
    host.settle();
    assert_eq!(
        host.quits, 1,
        "the quit is authorized once the engine stopped"
    );

    host.key(Action::ToggleWrap, false);
    host.key(Action::ToggleApprove, false);
    let released = host.try_update(|model| {
        model.set_sidebar_width(300.0);
        model.remember_layout();
    });
    assert!(released.is_none(), "the divider's release acts");
    host.try_update(|model| model.tree_fold_all(true));
    assert!(host.model().tree_collapsed.is_empty(), "the folders fold");

    assert!(!host.model().look().wrap, "no key acts");
    assert!(host.model().pending.is_empty(), "nothing is marked");
    assert!(host.jobs.is_empty(), "nothing is saved");
    assert!(!config.exists());
}

/// Quitting waits for the settings still being written to the configuration file.
#[test]
fn quitting_waits_for_the_configuration_writes() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.settings.config_path = Some(config.clone()));

    host.update(|model| model.apply_wrap(true));
    host.update(|model| model.quit(QuitKind::Interactive));
    assert_eq!(host.quits, 0, "nothing quits while a write waits");
    assert_eq!(host.model().status, "Saving your settings before quitting…");

    host.settle();
    assert_eq!(host.quits, 1);
    assert!(
        crate::config::parse(&std::fs::read_to_string(&config).unwrap())
            .unwrap()
            .wrap
    );
}

/// The sheet keys ask the front end to scroll the sheet it lays out, a line or half its
/// height at a time.
#[test]
fn the_sheet_keys_scroll_the_front_ends_sheet() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::ToggleHelp);
    for action in [
        Action::SheetDown,
        Action::SheetUp,
        Action::SheetPageDown,
        Action::SheetPageUp,
    ] {
        host.dispatch(action);
    }
    assert_eq!(
        host.sheet_steps,
        [
            SheetStep::Lines(1),
            SheetStep::Lines(-1),
            SheetStep::HalfPages(1),
            SheetStep::HalfPages(-1),
        ]
    );
}

/// The badges of hunk `hunk` of the shown file, pending verdicts included.
fn hunk_badges(model: &Model, hunk: usize) -> Vec<rows::Badge> {
    let view = model.view().unwrap();
    let units = model
        .rows
        .hunks
        .get(hunk)
        .map(|hunk| vec![hunk.unit])
        .unwrap_or_default();
    state::badges(&view, &units, &model.pending)
}

/// The file finder counts a file's automatic approvals as done, as the tree does: the demo's
/// `src/parser.rs`, whose hunks are all proven formatting-only, reads as finished.
#[test]
fn the_file_finder_counts_automatic_approvals() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::FindFiles);
    host.settle();
    host.update(|model| model.edit_field(Field::PickerQuery, "parser"));
    let search = host.run_job();
    host.complete(search);
    let picker = host.model().picker.as_ref().unwrap();
    let parser = picker
        .entries
        .iter()
        .find(|entry| entry.path == "src/parser.rs")
        .and_then(|entry| entry.change)
        .unwrap();
    assert!(parser.total > 0);
    assert_eq!(parser.reviewed, parser.total);
}

/// Proven formatting-only hunks that no mark decides count as done without a mark: the diff
/// opens at the first hunk that needs one, past the engine's brace reflow, and approving that
/// hunk finishes the file. Space on the reflow records an explicit reset, which makes it need
/// review; the setting turned off reopens every other such hunk.
#[test]
fn formatting_only_hunks_are_approved_automatically() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    // The demo accepted the first hunk; the reflow, the second, needs no mark.
    assert_eq!(host.model().hunk, 2, "the diff opens past the reflow");
    assert_eq!(hunk_badges(host.model(), 1), [rows::Badge::Auto]);
    let (reviewed, _) = host.model().totals();
    assert_eq!(host.model().auto_approved(), 6);

    // Approving the one open hunk finishes the file.
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert!(!host.model().file_open(), "the file needs nothing more");
    assert_eq!(host.model().totals().0, reviewed + 1);

    // Space on the reflow resets it explicitly, and it needs review again.
    host.dispatch(Action::PreviousHunk);
    host.dispatch(Action::ToggleApprove);
    assert!(
        host.model()
            .pending
            .values()
            .all(|&verdict| verdict == MarkVerdict::Unseen)
    );
    host.settle();
    assert_eq!(hunk_badges(host.model(), 1), [rows::Badge::Unreviewed]);
    assert!(host.model().file_open());
    assert_eq!(host.model().auto_approved(), 5);

    // Turned off, the other formatting-only hunks need review too; nothing was marked.
    host.update(Model::open_settings);
    host.update(|model| model.click_setting(settings::Choice::AutoApprove(false)));
    host.update(Model::close_settings);
    assert_eq!(host.model().auto_approved(), 0);
    assert_eq!(host.model().totals().0, reviewed + 1 - 6);
    let parser = host
        .model()
        .tree
        .iter()
        .find(|node| node.path == "src/parser.rs")
        .unwrap();
    assert_eq!(state::Progress::of(&parser.counts), state::Progress::Open);
}

/// The path of the shown file.
fn shown(model: &Model) -> &str {
    model.selected.as_deref().unwrap_or_default()
}

/// `n` and `N` step through the files in the visible tree's order: a folded folder's files are
/// skipped whole and folders are no stops, the diff shows each file at its first open hunk,
/// either end stays put with a note, and while a search is in the diff they step its matches.
#[test]
fn n_and_shift_n_step_through_the_visible_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let position = |model: &Model, path: &str| {
        model
            .visible_tree()
            .iter()
            .position(|&index| model.tree[index].path == path)
            .unwrap()
    };

    // In the tree, past the folded `src/` from the file before it to the one after.
    host.update(|model| {
        model.tree_collapsed.insert("src".to_owned());
        model.update_visible_tree();
        model.tree_cursor = position(model, "scripts/release.sh");
    });
    host.dispatch(Action::NextFile);
    host.settle();
    assert_eq!(
        path_at(host.model(), host.model().tree_cursor),
        "tools/notify.rb"
    );
    assert_eq!(shown(host.model()), "tools/notify.rb", "the file shows");
    host.dispatch(Action::PreviousFile);
    host.settle();
    assert_eq!(
        path_at(host.model(), host.model().tree_cursor),
        "scripts/release.sh"
    );

    // In the diff, the next and previous files show, each at its first open hunk.
    host.update(|model| model.tree_collapsed.clear());
    host.update(Model::update_visible_tree);
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextFile);
    assert_eq!(shown(host.model()), "src/format.rs");
    assert_eq!(host.model().mode, Mode::Diff);
    host.dispatch(Action::PreviousFile);
    assert_eq!(shown(host.model()), "src/engine.rs");
    assert_eq!(
        host.model().hunk,
        2,
        "at its first hunk that needs the reviewer"
    );

    // The ends stay put.
    host.update(|model| model.show_file("server.go"));
    host.dispatch(Action::NextFile);
    assert_eq!(shown(host.model()), "server.go");
    assert_eq!(host.model().status, "This is the last file.");

    // A search in the diff takes `n` and `N` for its matches.
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::OpenFind);
    host.update(|model| model.edit_field(Field::Find, "balance"));
    host.update(|model| model.submit_field(Field::Find));
    let current = host.model().find.as_ref().unwrap().current;
    host.dispatch(Action::NextFile);
    assert_eq!(shown(host.model()), "src/engine.rs");
    assert_ne!(host.model().find.as_ref().unwrap().current, current);
    host.dispatch(Action::PreviousFile);
    assert_eq!(host.model().find.as_ref().unwrap().current, current);
}

/// Tab and Shift+Tab switch between the files and the code: the tree focuses the diff, and the
/// diff and the buffer go back to the tree.
#[test]
fn tab_switches_between_the_files_and_the_code() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    assert_eq!(host.model().mode, Mode::Tree);
    host.dispatch(Action::SwitchPane);
    assert_eq!(host.model().mode, Mode::Diff);
    host.dispatch(Action::SwitchPane);
    assert_eq!(host.model().mode, Mode::Tree);
    assert_eq!(
        path_at(host.model(), host.model().tree_cursor),
        "src/engine.rs"
    );
    host.dispatch(Action::SwitchPane);
    host.dispatch(Action::ToggleBuffer);
    assert_eq!(host.model().mode, Mode::Buffer);
    host.dispatch(Action::SwitchPane);
    assert_eq!(host.model().mode, Mode::Tree);

    // Both keys are bound in each mode, Shift+Tab as Tab.
    for area in [keymap::Area::Tree, keymap::Area::Diff, keymap::Area::Buffer] {
        let keys: Vec<&str> = host
            .model()
            .settings
            .keymap
            .iter()
            .filter(|binding| binding.area == area && binding.action == "SwitchPane")
            .flat_map(|binding| binding.keys.iter().map(String::as_str))
            .collect();
        assert_eq!(keys, ["tab", "shift-tab"], "{area:?}");
    }
}

/// The bottom bar's left side as text, given `budget` cells.
fn sync_text(model: &Model, budget: f32) -> String {
    let palette = crate::paint::Palette::of(model);
    crate::paint::bars::sync_label(
        model,
        &palette,
        std::time::SystemTime::now(),
        budget,
        |text| scroll::float(text.chars().count()),
    )
    .into_iter()
    .map(|piece| piece.text)
    .collect()
}

/// The bar names the revision on screen, its head commit and when the review last synced; a
/// narrow bar drops the commit first, then the time; a fetch shows what it is doing until it
/// ends, a failed one says so for a moment, and the minute clock keeps going by itself.
#[test]
fn the_bar_says_which_revision_shows_and_how_fresh_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let head = host
        .model()
        .view()
        .and_then(|view| {
            view.record()
                .map(|record| record.head.as_str()[..7].to_owned())
        })
        .unwrap();
    assert_eq!(
        sync_text(host.model(), 100.0),
        format!("rev 1 · {head} · synced just now")
    );
    assert_eq!(sync_text(host.model(), 24.0), "rev 1 · synced just now");
    assert_eq!(sync_text(host.model(), 10.0), "rev 1");

    // A fetch says what it does while it runs, and when it synced once it ends.
    host.dispatch(Action::Refresh);
    assert!(sync_text(host.model(), 100.0).ends_with("syncing…"));
    let fetched = host.run_job();
    host.complete(fetched);
    host.settle();
    assert!(sync_text(host.model(), 100.0).ends_with("synced just now"));

    // A failure shows for a moment, then the last good sync again.
    host.update(Model::sync_failed);
    assert!(sync_text(host.model(), 100.0).ends_with("sync failed"));
    host.update(|model| {
        model.sync.failed_at =
            Some(std::time::SystemTime::now() - sync::FAILURE_SHOWS - Duration::from_secs(1));
    });
    assert!(sync_text(host.model(), 100.0).ends_with("synced just now"));

    // The minute clock asks for its next tick each time it ticks.
    assert_eq!(host.clocks.len(), 1);
    let tick = host.clocks.remove(0);
    host.complete(tick);
    assert_eq!(host.clocks.len(), 1);

    // A newer revision recorded elsewhere waits on the bar until the reviewer switches to it.
    prepare(
        &DemoPaths::under(dir.path()),
        &fixture_dir(),
        DemoRequest::UpTo(RevisionNumber::new(2).unwrap()),
    )
    .unwrap();
    host.dispatch(Action::Refresh);
    host.settle();
    assert_eq!(
        sync_text(host.model(), 100.0),
        format!("rev 1 · {head} · rev 2 available · synced just now")
    );
    assert_eq!(sync_text(host.model(), 30.0), "rev 1 · rev 2 available");
}

/// The file header says how long the file is, before its line counts, and drops that first
/// when it runs out of room.
#[test]
fn the_file_header_says_how_long_the_file_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.settle();
    let model = host.model();
    let palette = crate::paint::Palette::of(model);
    let header = |budget: f32| {
        let room = crate::paint::Room {
            budget,
            cell: 1.0,
            gap: 1.0,
        };
        crate::paint::header::file_header(model, &palette, room, 1.0).unwrap()
    };
    let engine = include_str!("../../../review-core/demo/trees/head-1/src/engine.rs");
    let wide = header(200.0);
    assert_eq!(
        wide.lines.map(|lines| lines.text),
        Some(format!("{} lines", engine.lines().count()))
    );
    let shown: Vec<(bool, bool)> = (0..200_u8)
        .map(|budget| {
            let header = header(f32::from(budget));
            (header.lines.is_some(), header.counts.is_some())
        })
        .collect();
    assert!(
        shown.iter().all(|&(lines, counts)| !lines || counts),
        "the length never shows without the counts"
    );
    assert!(
        shown.contains(&(false, true)),
        "a narrower header keeps the counts alone"
    );
}

/// Comfortable rows count as tall only where the front end draws them taller than normal ones,
/// as a window does; a terminal's rows never grow, so its comfortable density is roomy instead.
#[test]
fn only_growing_rows_are_tall() {
    let metrics = crate::frame::Metrics {
        code_size: 14.0,
        density: crate::config::Density::Comfortable,
        panel: None,
    };
    let terminal = Scale::fixed(cells());
    assert!(!Geometry::default().grid(&terminal, &metrics).tall_rows);
    let window = Scale::new(|metrics| Sizes {
        row_height: match metrics.density {
            crate::config::Density::Comfortable => 1.5,
            _ => 1.0,
        },
        ..cells()
    });
    assert!(Geometry::default().grid(&window, &metrics).tall_rows);
}

/// Enter on a folded approved hunk shows its code and folds it again, and `p` does the same,
/// as `z a` does.
#[test]
fn enter_and_p_show_a_folded_hunk() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.update(|model| model.select_hunk(0));
    let folded = |model: &Model| {
        let view = model.view().unwrap();
        model.hunk_units().iter().all(|&unit| {
            model
                .folds
                .is_folded(&crate::rows::Source::Verified(view), unit)
        })
    };
    assert!(
        folded(host.model()),
        "the approved first hunk starts folded"
    );
    host.dispatch(Action::Activate);
    assert!(!folded(host.model()), "Enter shows its code");
    host.dispatch(Action::Activate);
    assert!(folded(host.model()), "and folds it again");
    let fold_keys: Vec<&str> = host
        .model()
        .settings
        .keymap
        .iter()
        .filter(|binding| binding.action == "ToggleFold")
        .flat_map(|binding| binding.keys.iter().map(String::as_str))
        .collect();
    assert_eq!(fold_keys, ["z a", "p"]);
}

/// A review of `main..feature` in a new repository under `dir`: `main` holds `base`, and
/// `feature` changes it to `head`, each a list of paths and contents, a path renamed in `head`
/// under the same content as git sees it.
fn review_of<C: AsRef<[u8]>>(dir: &Path, base: &[(&str, C)], head: &[(&str, C)]) -> Review {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .envs(crate::testing::GIT_ENV)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    let write = |files: &[(&str, C)]| {
        for (path, content) in files {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    write(base);
    git(&["add", "--all"]);
    git(&["commit", "--quiet", "--allow-empty", "-m", "base"]);
    git(&["checkout", "--quiet", "-b", "feature"]);
    for entry in std::fs::read_dir(&repo).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != ".git" {
            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
    write(head);
    git(&["add", "--all"]);
    git(&["commit", "--quiet", "--allow-empty", "-m", "head"]);
    let review = Review::open(
        &dir.join("data"),
        review_core::Repo::discover(&repo).unwrap(),
        review_core::ReviewTarget::Range {
            base: "main".to_owned(),
            head: "feature".to_owned(),
        },
    )
    .unwrap();
    review.refresh().unwrap();
    review
}

/// The highlights of the version of `path` in the shown revision, head or base.
fn side_syntax(model: &Model, path: &str, head: bool) -> Option<Arc<SideSyntax>> {
    let view = model.view().unwrap();
    let analysis = view.analysis();
    analysis
        .files
        .iter()
        .filter_map(|file| {
            if head {
                file.new.as_ref()
            } else {
                file.old.as_ref()
            }
        })
        .find(|side| side.path == path)
        .and_then(|side| model.blobs.side_syntax(side))
}

/// The tone at the start of the zero-based `line` of `syntax`.
fn first_tone(syntax: &SideSyntax, line: usize) -> crate::syntax::Tone {
    syntax
        .line(line)
        .first()
        .filter(|(range, _)| range.start == 0)
        .map_or(crate::syntax::Tone::Plain, |(_, tone)| *tone)
}

/// One blob under two names is two file versions, each highlighted as its own name's
/// language, never as whichever was highlighted first.
#[test]
fn the_same_blob_under_two_names_highlights_as_each() {
    let dir = tempfile::tempdir().unwrap();
    let content = indoc! {"
        # heading
        echo hi
    "};
    let review = review_of(
        dir.path(),
        &[("keep.txt", "x\n")],
        &[
            ("keep.txt", "x\n"),
            ("notes.sh", content),
            ("notes.md", content),
        ],
    );
    let host = Host::open(review);
    let script = side_syntax(host.model(), "notes.sh", true).expect("the script is highlighted");
    let markdown =
        side_syntax(host.model(), "notes.md", true).expect("the Markdown is highlighted");
    assert_eq!(first_tone(&script, 0), crate::syntax::Tone::Comment);
    assert_eq!(first_tone(&markdown, 0), crate::syntax::Tone::Heading);
}

/// A rename across a language boundary keeps its blob, and each side takes its own path's
/// language: the base as a script, the head as Markdown.
#[test]
fn a_rename_across_languages_highlights_each_side_by_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let content = indoc! {"
        # heading
        echo hi
        more text
        and more
    "};
    let review = review_of(
        dir.path(),
        &[("notes.sh", content)],
        &[("notes.md", content)],
    );
    let host = Host::open(review);
    let base = side_syntax(host.model(), "notes.sh", false).expect("the base is highlighted");
    let head = side_syntax(host.model(), "notes.md", true).expect("the head is highlighted");
    assert_eq!(first_tone(&base, 0), crate::syntax::Tone::Comment);
    assert_eq!(first_tone(&base, 1), crate::syntax::Tone::Function);
    assert_eq!(first_tone(&head, 0), crate::syntax::Tone::Heading);
    assert_eq!(first_tone(&head, 1), crate::syntax::Tone::Plain);
}

/// A publish that holds only a renamed file's base still rebuilds the selected file, which
/// the tree shows under its new path, so its removed lines and deletion breadcrumbs take the
/// base's highlights.
#[test]
fn a_base_only_publish_rebuilds_a_renamed_file() {
    let dir = tempfile::tempdir().unwrap();
    let base = indoc! {"
        fn one() {}
        fn two() {}
        fn three() {}
        fn four() {}
        fn five() {}
        fn six() {}
    "};
    let head = base.replace("fn three() {}", "fn three() { 3 }");
    let review = review_of(
        dir.path(),
        &[("old.rs", base)],
        &[("new.rs", head.as_str())],
    );
    let mut host = Host::open(review);
    assert_eq!(host.model().selected.as_deref(), Some("new.rs"));
    let base_syntax = |model: &Model| {
        model
            .folds
            .syntax
            .get("new.rs")
            .and_then(|syntax| syntax.base.clone())
    };
    assert!(
        base_syntax(host.model()).is_some(),
        "both sides start highlighted"
    );

    // Only the base needs highlighting again, as after a rebase that moved only the base.
    host.update(|model| {
        let key = model
            .selected_sides()
            .into_iter()
            .find(|side| side.path == "old.rs")
            .unwrap();
        let resolved = model.blobs.resolved.remove(&key).unwrap();
        if let Some(made) = model.blobs.syntax.get_mut(&key.oid) {
            made.remove(&resolved);
        }
        model.rebuild();
    });
    assert!(base_syntax(host.model()).is_none());
    host.update(Model::pump_highlights);
    host.settle();
    assert!(
        base_syntax(host.model()).is_some(),
        "the base's highlights reach the file shown under its new path"
    );
}

/// A deleted script without an extension is read before its syntax is chosen, so its `#!`
/// line still makes it Bash.
#[test]
fn a_deleted_extensionless_script_is_highlighted_by_its_shebang() {
    let dir = tempfile::tempdir().unwrap();
    let review = review_of(
        dir.path(),
        &[
            ("keep.txt", "x\n"),
            (
                "tools/deploy",
                indoc! {"
                    #!/usr/bin/env -S bash -e
                    echo hi
                "},
            ),
        ],
        &[("keep.txt", "y\n")],
    );
    let host = Host::open(review);
    let base = side_syntax(host.model(), "tools/deploy", false).expect("the base is highlighted");
    assert_eq!(first_tone(&base, 0), crate::syntax::Tone::Comment);
    assert_eq!(first_tone(&base, 1), crate::syntax::Tone::Function);
}

/// A host over `review` whose highlighting has `workers` workers, whose opening job waits to
/// run.
fn open_unsettled(review: Review, workers: usize) -> Host {
    let mut host = Host::opening(loaded(review), settings());
    host.update(|model| model.highlights.workers = workers);
    host
}

/// The selected file is highlighted at once in a lane of its own while a worker takes the other
/// file, and each job lands on its own, in whichever order they end.
#[test]
fn the_selected_file_and_a_worker_highlight_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let review = review_of(
        dir.path(),
        &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")],
        &[("a.rs", "fn a() { 1 }\n"), ("b.rs", "fn b() { 2 }\n")],
    );
    let mut host = open_unsettled(review, 1);
    let opening = host.run_job();
    host.complete(opening);
    host.draw();
    let selected = host.model().selected.clone().unwrap();
    let other = if selected == "a.rs" { "b.rs" } else { "a.rs" };
    // The opening read the texts; both files' highlights start, the selected one's first.
    assert_eq!(
        host.jobs.len(),
        2,
        "the selected file's lane and the worker"
    );
    assert!(host.model().busy());

    // The worker's job ends first: the other file is highlighted, the selected one not yet.
    let worker = host.run_newest_job();
    host.complete(worker);
    assert!(side_syntax(host.model(), other, true).is_some());
    assert!(side_syntax(host.model(), &selected, true).is_none());
    assert!(host.model().busy(), "the selected file's job still runs");

    let lane = host.run_job();
    host.complete(lane);
    assert!(side_syntax(host.model(), &selected, true).is_some());
    assert!(host.jobs.is_empty(), "nothing is left to highlight");
    assert!(!host.model().busy());
}

/// A cancelled highlight keeps nothing, since what it made may be partial: moving on cancels
/// the selected file's job, and the file it had is highlighted again later.
#[test]
fn a_cancelled_highlight_is_highlighted_again() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        ("a.rs", "fn a() {}\n"),
        ("b.rs", "fn b() {}\n"),
        ("c.rs", "fn c() {}\n"),
    ];
    let changed = [
        ("a.rs", "fn a() { 1 }\n"),
        ("b.rs", "fn b() { 1 }\n"),
        ("c.rs", "fn c() { 1 }\n"),
    ];
    let mut host = open_unsettled(review_of(dir.path(), &files, &changed), 1);
    let opening = host.run_job();
    host.complete(opening);
    host.draw();
    let first = host.model().selected.clone().unwrap();
    assert_eq!(
        host.jobs.len(),
        2,
        "the selected file's lane and the worker"
    );

    // The first file's job makes its highlights, but the reviewer moves on to the last file
    // before it ends, and it reports itself cancelled, as when the flag comes during its last
    // version.
    let mut cancelled = host.run_job();
    match cancelled.outcome.as_mut() {
        Outcome::Highlighted(highlighted) => {
            assert!(!highlighted.syntax.is_empty(), "the job made highlights");
            highlighted.cancelled = true;
        }
        _ => panic!("the selected file's highlight job runs first"),
    }
    let last = model_files(host.model()).last().cloned().unwrap();
    assert_ne!(first, last);
    host.update(|model| model.show_file(&last));
    host.complete(cancelled);
    assert!(
        side_syntax(host.model(), &first, true).is_none(),
        "a cancelled job keeps nothing"
    );

    host.settle();
    for path in model_files(host.model()) {
        assert!(
            side_syntax(host.model(), &path, true).is_some(),
            "{path} is highlighted"
        );
    }
    assert!(!host.model().busy());
}

/// The blob of `path`'s head in the shown revision.
fn head_blob(model: &Model, path: &str) -> ObjectId {
    let view = model.view().unwrap();
    view.analysis()
        .files
        .iter()
        .filter_map(|file| file.new.as_ref())
        .find(|side| side.path == path)
        .unwrap()
        .oid
        .clone()
}

/// The files of the shown revision, in review order.
fn model_files(model: &Model) -> Vec<String> {
    file_paths(&model.tree)
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// A highlight that lands after a working-tree capture keeps the versions the capture still
/// has, so a file it did not touch is never highlighted twice, and drops the ones it replaced.
#[test]
fn a_highlight_landing_after_a_capture_keeps_what_the_capture_still_has() {
    let dir = tempfile::tempdir().unwrap();
    let (review, repo) = worktree_review_of(
        dir.path(),
        &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")],
        &[("a.rs", "fn a() { 1 }\n"), ("b.rs", "fn b() { 1 }\n")],
    );
    let mut host = Host::open(review);
    host.update(|model| model.show_file("a.rs"));
    host.settle();
    // A capture whose highlight jobs wait, the newest of the jobs it starts being b.rs's.
    let capture = |host: &mut Host| {
        host.update(|model| model.capture(false));
        let captured = host.run_job();
        host.complete(captured);
        host.draw();
    };

    // b.rs changes: its new head's highlight starts and is held back.
    std::fs::write(repo.join("b.rs"), "fn b() { 2 }\n").unwrap();
    capture(&mut host);
    assert!(side_syntax(host.model(), "b.rs", true).is_none());
    let held = host.jobs.pop_back().expect("b.rs's highlight waits");
    host.settle();

    // a.rs changes meanwhile; b.rs does not, so its version stays in flight with the held job.
    std::fs::write(repo.join("a.rs"), "fn a() { 2 }\n").unwrap();
    capture(&mut host);
    host.settle();
    assert!(side_syntax(host.model(), "a.rs", true).is_some());
    assert!(
        side_syntax(host.model(), "b.rs", true).is_none(),
        "no second job highlights b.rs"
    );
    let landed = held.run();
    host.complete(landed);
    assert!(
        side_syntax(host.model(), "b.rs", true).is_some(),
        "the held job's highlights belong to the capture shown now"
    );

    // b.rs changes again under a held job: its old head's highlights are dropped, and the new
    // head is highlighted by a job of its own.
    std::fs::write(repo.join("b.rs"), "fn b() { 3 }\n").unwrap();
    capture(&mut host);
    let old_head = head_blob(host.model(), "b.rs");
    let stale = host.jobs.pop_back().expect("b.rs's highlight waits");
    host.settle();
    std::fs::write(repo.join("b.rs"), "fn b() { 4 }\n").unwrap();
    capture(&mut host);
    let landed = stale.run();
    host.complete(landed);
    let blobs = &host.model().blobs;
    assert!(
        !blobs.syntax.contains_key(&old_head)
            && !blobs.resolved.keys().any(|side| side.oid == old_head),
        "the old head's highlights are not kept"
    );
    assert!(side_syntax(host.model(), "b.rs", true).is_none());
    host.settle();
    assert!(side_syntax(host.model(), "b.rs", true).is_some());
    assert!(!host.model().busy());
}

/// A review of the working tree of a repository in `dir`: `base` committed on `main`, and
/// `worktree` written over it, uncommitted. Returns the review and the working tree.
fn worktree_review_of(
    dir: &Path,
    base: &[(&str, &str)],
    worktree: &[(&str, &str)],
) -> (Review, std::path::PathBuf) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .envs(crate::testing::GIT_ENV)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    for (path, content) in base {
        std::fs::write(repo.join(path), content).unwrap();
    }
    git(&["add", "--all"]);
    git(&["commit", "--quiet", "-m", "base"]);
    git(&["checkout", "--quiet", "-b", "feature"]);
    for (path, content) in worktree {
        std::fs::write(repo.join(path), content).unwrap();
    }
    let review = Review::open(
        &dir.join("data"),
        review_core::Repo::discover(&repo).unwrap(),
        review_core::ReviewTarget::Worktree {
            base: "main".to_owned(),
        },
    )
    .unwrap();
    review.refresh().unwrap();
    (review, repo)
}

/// A file with a change near its top and one near its bottom, as `top` and `bottom`, which
/// may span lines.
fn two_hunks(top: &str, bottom: &str) -> String {
    let filler = "// filler\n".repeat(20);
    formatdoc! {"
        fn a() {{
            {top}
        }}
        {filler}fn z() {{
            {bottom}
        }}
    "}
}

/// The window over a live review of `a.rs` changed at its top and bottom, with the diff of
/// `a.rs` showing; returns it with the working tree.
fn live_host(dir: &Path) -> (Host, std::path::PathBuf) {
    live_host_with(dir, settings())
}

/// [`live_host`] with `settings`.
fn live_host_with(dir: &Path, settings: Settings) -> (Host, std::path::PathBuf) {
    let (review, repo) = worktree_review_of(
        dir,
        &[("a.rs", &two_hunks("1", "2"))],
        &[("a.rs", &two_hunks("one", "two"))],
    );
    let mut host = Host::open_with(review, settings);
    assert!(host.model().following(), "a working-tree review follows it");
    assert_eq!(host.waits.len(), 1, "the watch waits for changes");
    host.update(|model| model.show_file("a.rs"));
    host.dispatch(Action::FocusDiff);
    (host, repo)
}

/// Writes `a.rs` and captures the working tree, as a change the watch saw does.
fn edit_and_capture(host: &mut Host, repo: &Path, top: &str, bottom: &str) {
    std::fs::write(repo.join("a.rs"), two_hunks(top, bottom)).unwrap();
    host.update(|model| model.capture(false));
    host.settle();
}

/// A job queued on a capture reads it whatever the window shows meanwhile, and holds the review
/// for as long as it waits: while it waits to read a PDF the capture staged, the window moves on
/// two captures that stage other files, the PDF as it was, and closes, and another open finds
/// the archive due to be compacted, yet compacts nothing and the review cannot be dropped, so
/// the job still reads the PDF's pages; once the job let go, the next open compacts the capture
/// away.
///
/// The window captures only when asked, so no watch, which a thread of its own drops, holds the
/// review once the window closes.
#[cfg(feature = "pdf")]
#[test]
fn a_queued_job_holds_its_capture_until_it_ends() {
    use crate::pdf::testing::pdf;

    let dir = tempfile::tempdir().unwrap();
    let (loaded, repo) = open_checkout(dir.path());
    let mut host = Host::open_loaded(
        loaded,
        Settings {
            live: false,
            ..settings()
        },
    );
    // The PDF goes into the index, so the capture takes its blob in a pack from the repository.
    let stage = |path: &str, content: &[u8]| {
        std::fs::write(repo.join(path), content).unwrap();
        let added = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", path])
            .envs(crate::testing::GIT_ENV)
            .output()
            .unwrap();
        assert!(added.status.success(), "git add: {added:?}");
    };
    stage("doc.pdf", &pdf(&["First"]));
    host.update(|model| model.capture(false));
    while !host.model().publishes() {
        let completion = host.run_job();
        host.complete(completion);
    }
    host.draw();
    let capture = host.model().view().unwrap().record().unwrap().head.clone();
    host.update(|model| model.show_file("doc.pdf"));
    let queued = std::mem::take(&mut host.jobs);

    // The PDF stays the pair the window shows, so its read is still wanted.
    for other in ["Second", "Third"] {
        stage("other.txt", other.as_bytes());
        host.update(|model| model.capture(false));
        host.settle();
    }
    let data = dir.path().join("data");
    let key = host.review.key().clone();
    let archive = host.review.snapshots().git_dir().to_path_buf();
    drop(host);
    std::fs::remove_file(archive.join("compacted")).unwrap();
    let other = Review::load(&data, &key).unwrap();
    assert!(
        !archive.join("compacted").exists(),
        "an open compacted while a job held the review"
    );
    assert!(other.snapshots().tree_of(&capture).is_ok());
    drop(other);
    assert!(matches!(
        Review::remove(&data, &key),
        Err(review_core::Error::ReviewInUse(_))
    ));

    let mut read = None;
    for job in queued {
        let completion = job.run();
        if let Outcome::PdfRead(pdf) = &*completion.outcome {
            read = Some(
                pdf.as_ref()
                    .is_some_and(|pdf| matches!(pdf.read, super::pages::Read::Pages(_))),
            );
        }
    }
    assert_eq!(read, Some(true), "the job read the pages of its capture");
    let maintained = Review::load(&data, &key).unwrap();
    assert!(archive.join("compacted").exists(), "nothing compacted");
    assert!(
        maintained.snapshots().tree_of(&capture).is_err(),
        "the capture outlived every holder"
    );
}

/// A capture refused for want of space keeps the view and says so once, however often the
/// periodic capture runs into it, and again after a capture showed in between.
#[test]
fn a_full_data_directory_says_so_once() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    let full = || {
        Err(super::live::Missed::Full(
            "storage full: run `cr drop`".to_owned(),
        ))
    };
    let shown = host.model().view().map(|view| view.number());

    host.update(|model| model.captured(full()));
    assert_eq!(host.model().status, "storage full: run `cr drop`");
    host.update(|model| model.notice("elsewhere"));
    host.update(|model| model.captured(full()));
    assert_eq!(host.model().status, "elsewhere", "it said so once");
    assert_eq!(host.model().view().map(|view| view.number()), shown);

    edit_and_capture(&mut host, &repo, "again", "two");
    host.update(|model| model.captured(full()));
    assert_eq!(host.model().status, "storage full: run `cr drop`");
}

/// The text the selected hunk adds.
fn selected_text(model: &Model) -> String {
    let view = model.view().unwrap();
    model
        .hunk_units()
        .iter()
        .filter_map(|&unit| {
            view.unit(unit)
                .ok()?
                .chunk()
                .map(|chunk| chunk.added.clone())
        })
        .collect()
}

/// An edit shows at once in place of the snapshot on screen, as a provisional revision, with
/// the reviewer still on the same hunk of the same file and their fold kept, although the hunk
/// moved down.
#[test]
fn a_live_review_shows_an_edit_and_keeps_the_place() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    host.dispatch(Action::NextHunk);
    assert!(selected_text(host.model()).contains("two"));
    host.dispatch(Action::ToggleFold);
    let shown = host.model().shown;

    edit_and_capture(&mut host, &repo, &["one", "    more"].join("\n"), "two");

    let model = host.model();
    let view = model.view().unwrap();
    assert!(
        view.is_provisional(),
        "the capture shows without being recorded"
    );
    assert_eq!(model.shown, shown + 1);
    assert_eq!(model.selected.as_deref(), Some("a.rs"));
    assert_eq!(model.hunk, 1);
    assert!(selected_text(model).contains("two"));
    assert_eq!(
        model.folds.units,
        model.hunk_units().into_iter().collect(),
        "the folded hunk stays folded"
    );
}

/// A capture that only brings the working tree's edits leaves the status line alone, so the
/// outcome of the reviewer's last action stays readable.
#[test]
fn a_capture_keeps_the_outcome_of_the_last_action() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    host.update(|model| model.fail("Could not record the approval."));
    edit_and_capture(&mut host, &repo, "uno", "two");
    assert!(
        selected_text(host.model()).contains("uno"),
        "the capture shows"
    );
    assert_eq!(host.model().status, "Could not record the approval.");
}

/// What the status line said about how the snapshot on screen stood, such as that every file
/// is done, clears once another capture shows, so it never stays under a hunk an edit reopened.
#[test]
fn a_capture_clears_what_the_status_said_about_the_last_one() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert_eq!(host.model().totals(), (2, 2));
    assert!(host.model().status.starts_with("This file is done"));
    edit_and_capture(&mut host, &repo, "uno", "two");
    assert_eq!(host.model().totals(), (1, 2));
    assert_eq!(host.model().status, "");
}

/// A refresh the reviewer asked for answers only while its request still shows: an outcome
/// the reviewer caused meanwhile stays.
#[test]
fn a_refresh_never_answers_over_a_newer_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    host.update(Model::recapture);
    host.update(|model| model.set_status("Approved a.rs:1."));
    host.settle();
    assert_eq!(host.model().status, "Approved a.rs:1.");

    host.update(Model::recapture);
    host.settle();
    assert!(
        host.model().status.starts_with("Up to date."),
        "{}",
        host.model().status
    );
    // Its progress is about the snapshot it answered on, so it clears with the next capture.
    edit_and_capture(&mut host, &repo, "uno", "two");
    assert_eq!(host.model().status, "");
}

/// A review key on a hunk that just changed marks it at once, as the last frame showed it;
/// marking records the capture it was made on.
#[test]
fn a_review_key_on_a_hunk_that_just_changed_marks_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    edit_and_capture(&mut host, &repo, "uno", "two");
    assert!(selected_text(host.model()).contains("uno"));

    host.dispatch(Action::ToggleApprove);
    assert!(!host.model().pending.is_empty(), "the mark is queued");
    host.settle();
    let review = host.review.clone();
    let recorded = review.history().unwrap();
    assert_eq!(recorded.revisions().len(), 2, "the capture was recorded");
    assert!(!host.model().view().unwrap().is_provisional());
}

/// A capture that lands after the last frame shows with the next one: a key pressed before
/// that acts on what the last frame showed, and the capture taken again once that mark saved
/// shows the code that changed since as still to review.
#[test]
fn a_key_before_the_next_frame_acts_on_what_the_last_frame_showed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    edit_and_capture(&mut host, &repo, "uno", "two");
    let shown = host.model().shown;

    std::fs::write(repo.join("a.rs"), two_hunks("tres", "two")).unwrap();
    host.update(|model| model.capture(false));
    while let Some(job) = host.jobs.pop_front() {
        let completion = job.run();
        host.complete(completion);
    }
    assert!(
        host.model().publishes(),
        "the capture waits for the next frame"
    );
    assert_eq!(host.model().shown, shown);
    assert!(selected_text(host.model()).contains("uno"));

    host.dispatch(Action::ToggleApprove);
    host.settle();
    let review = host.review.clone();
    let history = review.history().unwrap();
    let recorded = history.current_view().unwrap();
    assert!(
        recorded.unit_ids().into_iter().any(|unit| {
            recorded
                .unit(unit)
                .ok()
                .and_then(|unit| unit.chunk().cloned())
                .is_some_and(|chunk| chunk.added.contains("uno"))
                && matches!(
                    recorded.state(unit),
                    Ok(review_core::UnitState::Reviewed {
                        verdict: review_core::Verdict::Accepted,
                        ..
                    })
                )
        }),
        "the mark is on the code the last frame showed"
    );
    let view = host.model().view().unwrap();
    let tres = view
        .unit_ids()
        .into_iter()
        .find(|&unit| {
            view.unit(unit)
                .ok()
                .and_then(|unit| unit.chunk().cloned())
                .is_some_and(|chunk| chunk.added.contains("tres"))
        })
        .expect("the capture shows");
    assert!(
        !matches!(
            view.state(tres),
            Ok(review_core::UnitState::Reviewed { .. })
        ),
        "the code that changed since is still to review"
    );
}

/// Opening the review starts no timer, so none ticks on once it opened: the front ends draw
/// the loading view's frames themselves for as long as it opens.
#[test]
fn opening_starts_no_timer() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::opening(loaded(demo(dir.path())), settings());
    assert!(host.timers.is_empty(), "nothing ticks while it opens");
    let opened = host.run_job();
    host.complete(opened);
    host.draw();
    assert!(matches!(host.model().phase, Phase::Ready));
    assert!(host.timers.is_empty(), "nothing ticks once it opened");
}

/// The review shows with the first frame after it opened, as a capture does: a key handled
/// before that frame, such as one in the same batch as the opening's completion, acts on the
/// loading view, where nothing is to review, and never marks the rows the frame then shows.
#[test]
fn a_key_before_the_first_frame_marks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::opening(loaded(demo(dir.path())), marks_only());
    let opened = host.run_job();
    host.complete(opened);
    assert!(
        matches!(host.model().phase, Phase::Loading),
        "the review waits for the next frame"
    );
    assert!(host.model().publishes());

    host.key(Action::ToggleApprove, false);
    assert_eq!(host.model().status, "Nothing here to review.");
    assert!(host.model().pending.is_empty(), "nothing is marked");
    assert!(host.jobs.is_empty(), "nothing is saved");

    host.draw();
    assert!(matches!(host.model().phase, Phase::Ready));
    assert!(host.model().pending.is_empty());
    host.key(Action::ToggleApprove, false);
    assert!(
        !host.model().pending.is_empty(),
        "a key after the frame marks what it showed"
    );
}

/// A key held down since before a hunk changed never marks it, however long it repeats; a new
/// press does.
#[test]
fn a_key_held_across_a_change_never_marks_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    edit_and_capture(&mut host, &repo, "uno", "two");
    host.update(Model::hold);
    edit_and_capture(&mut host, &repo, "tres", "two");
    host.key(Action::ToggleApprove, true);
    assert!(host.model().status.starts_with("Not marked"), "still held");
    assert!(host.model().saves.is_empty());

    host.dispatch(Action::ToggleApprove);
    assert!(!host.model().pending.is_empty(), "a new press marks");
}

/// A capture waits while a mark is saved, and is taken again afterwards, since the mark may
/// have recorded the snapshot it was compared with.
#[test]
fn a_capture_waits_for_marks_and_is_taken_again() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleApprove);
    assert!(!host.model().saves.is_empty() || host.model().saving);

    std::fs::write(repo.join("a.rs"), two_hunks("uno", "two")).unwrap();
    // Only the capture runs: the mark's save waits behind it.
    host.update(|model| model.capture(false));
    let capture = host.run_newest_job();
    host.complete(capture);
    assert!(host.model().live.as_ref().unwrap().has_waiting());
    assert!(!selected_text(host.model()).contains("uno"));

    host.settle();
    let model = host.model();
    assert!(!model.live.as_ref().unwrap().has_waiting());
    let view = model.view().unwrap();
    assert!(view.is_provisional());
    assert!(
        view.unit_ids()
            .iter()
            .filter_map(|&unit| view.unit(unit).ok()?.chunk())
            .any(|chunk| chunk.added.contains("uno"))
    );
    let two = view
        .unit_ids()
        .into_iter()
        .find(|&unit| {
            view.unit(unit)
                .ok()
                .and_then(|unit| unit.chunk())
                .is_some_and(|chunk| chunk.added.contains("two"))
        })
        .unwrap();
    assert!(view.state(two).unwrap().is_reviewed(), "the mark carried");
}

/// A capture compared with a snapshot that is no longer on screen never lands: it is taken
/// again from the snapshot that shows.
#[test]
fn a_capture_of_another_snapshot_is_taken_again() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    let shown = host.model().shown;
    host.dispatch(Action::ToggleHelp);
    edit_and_capture(&mut host, &repo, "uno", "two");
    assert!(
        host.model().live.as_ref().unwrap().has_waiting(),
        "help holds it back"
    );

    // A second capture is compared with the snapshot still on screen...
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "dos")).unwrap();
    host.update(|model| model.capture(false));
    // ...which the first replaces with the frame after help closes.
    host.dispatch(Action::CloseOverlay);
    host.draw();
    assert_eq!(host.model().shown, shown + 1);
    assert!(selected_text(host.model()).contains("uno"));

    host.settle();
    let model = host.model();
    assert_eq!(model.shown, shown + 2, "the stale capture was taken again");
    let view = model.view().unwrap();
    assert!(
        view.unit_ids()
            .iter()
            .filter_map(|&unit| view.unit(unit).ok()?.chunk())
            .any(|chunk| chunk.added.contains("dos"))
    );
    // Compared with the snapshot on screen, the top hunk did not change, so it takes a mark;
    // compared with the one before, it would have, and would refuse it.
    host.dispatch(Action::FirstHunk);
    assert!(selected_text(host.model()).contains("uno"));
    host.dispatch(Action::ToggleApprove);
    assert!(!host.model().pending.is_empty(), "{}", host.model().status);
}

/// A change on disk reaches the window through the watch: its wait ends, and the capture it
/// starts shows the change.
#[test]
fn the_watch_brings_a_change_to_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "two")).unwrap();
    // The watch says a change started, then that it settled.
    for _ in 0..2 {
        let wait = host.waits.pop().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(wait.run()).ok());
        let waited = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the watch sees the change");
        host.complete(waited);
        assert_eq!(host.waits.len(), 1, "the next wait starts");
    }
    host.settle();
    assert!(selected_text(host.model()).contains("uno"));
}

/// Once the watch could not watch a directory, as when the system's limit on watches is
/// reached, captures look twice, as without a watch.
#[cfg(target_os = "linux")]
#[test]
fn a_watch_short_of_the_tree_confirms_its_captures() {
    use std::os::unix::fs::DirBuilderExt as _;

    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    let covers_all = |host: &Host| {
        host.model()
            .parts()
            .watch
            .lock()
            .started()
            .is_some_and(|watch| watch.covers_all())
    };
    assert!(covers_all(&host));
    std::fs::DirBuilder::new()
        .mode(0o000)
        .create(repo.join("sealed"))
        .unwrap();
    if std::fs::read_dir(repo.join("sealed")).is_ok() {
        eprintln!("skipped: permissions do not apply to this user");
        return;
    }
    // The watch says a change started, then that it settled, having tried to watch the new
    // directory.
    for _ in 0..2 {
        let wait = host.waits.pop().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(wait.run()).ok());
        let waited = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the watch sees the directory");
        host.complete(waited);
    }
    assert!(!covers_all(&host), "captures look twice");
}

/// Reviews of a range never follow anything, and `[review] live = false` turns following off.
#[test]
fn only_a_working_tree_review_follows_it() {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::open(demo(dir.path()));
    assert!(!host.model().following());

    let dir = tempfile::tempdir().unwrap();
    let (review, _) = worktree_review_of(
        dir.path(),
        &[("a.rs", "fn a() {}\n")],
        &[("a.rs", "fn a() { 1 }\n")],
    );
    let host = Host::open_with(
        review,
        Settings {
            live: false,
            ..settings()
        },
    );
    assert!(!host.model().following());
    assert!(host.waits.is_empty());
}

/// A review of the working tree against a fixed ref shares the checkout's one grant, which
/// covers the code the checkout holds after later edits.
#[test]
fn a_working_tree_review_shares_the_checkouts_trust() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    trust(&mut host);
    assert!(
        host.model().status.starts_with("Trusted this checkout"),
        "{}",
        host.model().status
    );
    assert!(checkout_review(&host).trusted().unwrap());

    edit_and_capture(&mut host, &repo, "uno", "two");
    assert_eq!(host.model().trust, Trust::Granted, "the grant covers edits");
}

/// The review of the working tree's checkout, which holds the trust in it.
fn checkout_review(host: &Host) -> Review {
    host.review
        .checkout_review()
        .unwrap()
        .expect("the review has a checkout")
}

/// A review of a range has no code on disk: go to definition says so before it asks for trust,
/// and `Ctrl+T` says why there is nothing to trust; nothing runs.
#[test]
fn a_review_without_code_on_disk_asks_for_no_trust() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleBuffer);
    host.dispatch(Action::NextWord);
    assert!(!crate::paint::bars::hint_available(
        host.model(),
        "GoToDefinition"
    ));

    host.dispatch(Action::GoToDefinition);
    let model = host.model();
    assert_eq!(model.overlay, None);
    assert!(!model.looking_up);
    assert!(host.jobs.is_empty(), "a lookup runs");
    assert!(
        model
            .status
            .starts_with("Go to definition needs the code on disk. Language servers run only"),
        "{}",
        model.status
    );

    host.dispatch(Action::ToggleTrust);
    let model = host.model();
    assert_eq!(model.overlay, None);
    assert_eq!(model.trust, Trust::Denied);
    assert!(host.jobs.is_empty(), "a decision is saved");
    assert!(
        model
            .status
            .starts_with("Language servers run only in a checkout"),
        "{}",
        model.status
    );
}

/// Whether the process `id` has stopped: gone, or a zombie that only its owner's wait reaps.
#[cfg(target_os = "linux")]
fn stopped(id: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{id}/stat")).map_or(true, |stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
    })
}

/// A language server runs in the checkout across the captures that show, so it indexes once,
/// and a lookup re-syncs the file it asks about instead.
#[cfg(target_os = "linux")]
#[test]
fn a_server_outlives_the_captures_that_show() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    trust(&mut host);
    let (held, process) = serve(&mut host, "rust", dir.path());
    drop(held);

    edit_and_capture(&mut host, &repo, "uno", "two");
    edit_and_capture(&mut host, &repo, "eins", "two");
    assert_eq!(host.model().trust, Trust::Granted);
    assert_eq!(host.model().parts().servers.len(), 1);
    assert!(!stopped(process), "a capture stopped the server");
}

/// When a capture reads the trust again and finds it gone, as when another window revoked
/// it, the language servers stop as they do on a revocation here.
#[cfg(target_os = "linux")]
#[test]
fn trust_lost_elsewhere_stops_the_servers() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    trust(&mut host);
    let (held, process) = serve(&mut host, "rust", dir.path());
    drop(held);

    // Another window revokes the grant.
    host.review.set_trusted(false).unwrap();
    host.update(Model::changed);
    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);
    assert_eq!(host.model().parts().servers.len(), 0);
    assert!(stopped(process));
}

/// A revocation stops lookups at the key press: one that started before and reaches the
/// launch of its language server after the press starts none, though the grant stays stored
/// until the revocation is recorded, and its answer says nothing.
#[test]
fn a_revocation_stops_lookups_at_the_key_press() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    trust(&mut host);
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    let lookup = host.jobs.pop_back().expect("the lookup waits");
    host.dispatch(Action::ToggleTrust);
    assert_eq!(host.model().trust, Trust::Checking);

    // The lookup reaches the launch before the revocation is recorded.
    let checkout = checkout_review(&host);
    assert!(checkout.trusted().unwrap(), "the grant is stored");
    let answered = lookup.run();
    assert!(
        matches!(
            &*answered.outcome,
            Outcome::Definition(Err(super::buffer::Unanswered::Stale))
        ),
        "the lookup stopped before the launch"
    );
    assert_eq!(host.model().parts().servers.len(), 0);
    host.complete(answered);
    assert!(!host.model().looking_up);
    assert_eq!(
        host.model().status,
        "Revoking trust…",
        "the answer says nothing"
    );

    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);
    assert!(!checkout.trusted().unwrap());
}

/// A definition asked for before a revocation never lands, even once trust is granted again
/// in the same buffer and snapshot: the trust epoch it was asked in is gone.
#[test]
fn a_definition_from_before_a_revocation_never_lands_after_a_regrant() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    trust(&mut host);
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    let asked = lookup(host.model());
    // The lookup would start a language server, so it never runs here.
    drop(host.jobs.pop_back());
    host.update(|model| model.looking_up = false);
    host.dispatch(Action::ToggleTrust);
    host.settle();
    trust(&mut host);

    host.update(|model| model.looking_up = true);
    host.complete(found_for(asked));
    let model = host.model();
    assert!(!model.looking_up);
    assert!(
        model.buffer.as_ref().unwrap().previous.is_none(),
        "the answer from before the revocation landed"
    );
}

/// Trust found denied, as when another window revoked it, stops the language servers before
/// the window shows the denial, so none from before it is there to use once trust is granted
/// again on the same code, and nothing that runs late stops one started after.
#[cfg(target_os = "linux")]
#[test]
fn a_denial_stops_the_servers_before_it_shows() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    trust(&mut host);
    let (held, before) = serve(&mut host, "rust", dir.path());
    drop(held);

    // Another window revokes the grant, and a capture of the same code shows.
    host.review.set_trusted(false).unwrap();
    host.update(Model::changed);
    let captured = host.run_newest_job();
    host.complete(captured);
    host.draw();
    assert_eq!(
        host.model().trust,
        Trust::Granted,
        "the grant shows until it is read again"
    );
    let checked = host.run_newest_job();
    assert!(stopped(before), "the server from before the denial runs");
    host.complete(checked);
    assert_eq!(host.model().trust, Trust::Denied);

    // Trust is granted again, and a lookup starts the same server anew.
    trust(&mut host);
    let (held, after) = serve(&mut host, "rust", dir.path());
    drop(held);
    host.settle();
    assert!(
        !stopped(after),
        "the server started after the grant is stopped"
    );
}

/// A lookup that finds the stored grant gone, as another window revoked it, stops the language
/// servers before its answer lands, and the window shows trust denied.
#[cfg(target_os = "linux")]
#[test]
fn a_lookup_that_finds_trust_denied_stops_the_servers_first() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    trust(&mut host);
    buffer_on_a_name(&mut host);
    let (held, process) = serve(&mut host, "rust-analyzer", dir.path());
    drop(held);
    checkout_review(&host).set_trusted(false).unwrap();

    host.dispatch(Action::GoToDefinition);
    let answered = host.run_newest_job();
    assert!(stopped(process), "the server runs as the answer lands");
    host.complete(answered);
    assert_eq!(host.model().trust, Trust::Denied);
    assert!(
        host.model().status.contains("no longer applies"),
        "{}",
        host.model().status
    );
}

/// A wait for the working tree holds only what waits on the watch, never the engine: a window
/// dropped without stopping it, as on a panic, still lets go of the engine and the review,
/// which ends the wait.
#[cfg(target_os = "linux")]
#[test]
fn a_wait_for_the_working_tree_never_keeps_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    let parts = host.model().weak_parts();
    let wait = host.waits.pop().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(wait.run()).ok());

    drop(host);
    assert!(parts.upgrade().is_none(), "the engine outlived the window");
    let waited = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the wait ends");
    assert!(matches!(
        &*waited.outcome,
        Outcome::Waited(waited) if waited.waited == crate::watch::Waited::Closed
    ));
}

/// The window over a live review of `a.rs`, as [`live_host`] opens it, with the completion of
/// the job that started the watch, which kept the watch, held back before it lands.
fn live_host_holding_the_watch(dir: &Path) -> (Host, Completion) {
    let (review, _) = worktree_review_of(
        dir,
        &[("a.rs", &two_hunks("1", "2"))],
        &[("a.rs", &two_hunks("one", "two"))],
    );
    let mut host = Host::opening(loaded(review), settings());
    loop {
        if let Some(job) = host.jobs.pop_front() {
            let completion = job.run();
            if matches!(*completion.outcome, Outcome::Watching(Ok(_))) {
                return (host, completion);
            }
            host.complete(completion);
        } else if !host.timers.is_empty() {
            host.fire(0);
        } else {
            assert!(host.model().publishes(), "the watch starts");
            host.draw();
        }
    }
}

/// How many periodic captures of the working tree are armed.
fn reconciles(host: &Host) -> usize {
    host.clocks
        .iter()
        .filter(|clock| matches!(*clock.outcome, Outcome::Tick(super::sync::Tick::Reconcile)))
        .count()
}

/// A watch that finishes starting after the review ended in the window is dropped rather
/// than kept: the engine's slot for it stays closed, and what waits on it sees it closed.
#[test]
fn a_watch_that_starts_after_the_review_ended_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (review, _) = worktree_review_of(
        dir.path(),
        &[("a.rs", &two_hunks("1", "2"))],
        &[("a.rs", &two_hunks("one", "two"))],
    );
    // Without `[review] live` no watch starts, so the test starts one, as the job does before
    // it installs it.
    let mut host = Host::open_with(
        review,
        Settings {
            live: false,
            ..settings()
        },
    );
    let (watch, mut waiter) =
        crate::watch::Watch::start(host.model().parts().review.ignore_rules()).unwrap();

    host.update(|model| model.stop(Replaced::Log, false));
    host.settle();
    host.model().parts().install(watch);
    assert!(matches!(
        *host.model().parts().watch.lock(),
        super::engine::WatchSlot::Closed
    ));
    assert_eq!(waiter.wait(), crate::watch::Waited::Closed);
}

/// A watch that started before the review ended in the window, and whose start lands after,
/// arms nothing: no wait for changes and no periodic capture; the job that ends the review
/// drops the watch it kept.
#[test]
fn a_watch_that_lands_after_the_review_ended_arms_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, started) = live_host_holding_the_watch(dir.path());
    assert!(host.model().parts().watch.lock().started().is_some());

    // The start lands before the job that ends the review runs.
    host.update(|model| model.stop(Replaced::Log, false));
    host.complete(started);
    assert!(host.waits.is_empty(), "no wait for changes");
    assert_eq!(reconciles(&host), 0, "no periodic capture");

    host.settle();
    assert!(matches!(
        *host.model().parts().watch.lock(),
        super::engine::WatchSlot::Closed
    ));
}

/// Quitting closes the watch as the end of the review does: a change the watch saw that lands
/// while the engine stops arms no next wait, and the watch is gone once the app quits.
#[test]
fn a_change_that_lands_while_quitting_arms_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    let watch = Arc::downgrade(host.model().parts().watch.lock().started().unwrap());
    let armed = reconciles(&host);

    // The wait sees a change start, and its result is held back.
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "two")).unwrap();
    let wait = host.waits.pop().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(wait.run()).ok());
    let stirred = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the watch sees the change");
    assert!(matches!(
        &*stirred.outcome,
        Outcome::Waited(waited) if waited.waited == crate::watch::Waited::Stirred
    ));

    // Everything is saved and the job that stops the engine waits to run when the change
    // lands.
    host.update(|model| model.quit(QuitKind::Interactive));
    while !host.model().quitting.over() {
        let completion = host.run_job();
        host.complete(completion);
    }
    host.complete(stirred);
    assert!(host.waits.is_empty(), "no next wait for changes");
    assert_eq!(reconciles(&host), armed, "no other periodic capture");

    host.settle();
    assert_eq!(host.quits, 1);
    assert!(watch.upgrade().is_none(), "the watch stopped");
}

/// Once everything is saved for quitting and the engine stops, no input acts and no capture
/// shows, so nothing the reviewer does then goes unsaved; the engine stops without waiting for
/// a job still running, which keeps the review until it lets go.
#[cfg(target_os = "linux")]
#[test]
fn closing_takes_no_input_and_waits_for_no_capture() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    // A job still runs with the engine's parts, and a capture lands while the engine stops.
    let parts = host.model().weak_parts();
    let running = parts.upgrade();
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "two")).unwrap();
    host.update(|model| model.capture(false));
    let captured = host.run_newest_job();

    host.dispatch(Action::Quit);
    while host.model().quitting != Quitting::Closing {
        let saved = host.run_job();
        host.complete(saved);
    }
    assert_eq!(host.model().status, "Closing the review before quitting…");
    host.key(Action::ToggleWrap, false);
    host.key(Action::ToggleApprove, false);
    host.key(Action::CloseAllFolds, false);
    host.complete(captured);
    host.draw();
    let model = host.model();
    assert!(!model.look().wrap, "a key acts");
    assert!(model.pending.is_empty(), "a mark is taken");
    assert!(model.tree_collapsed.is_empty(), "a folder folds");
    assert!(!selected_text(model).contains("uno"), "the capture shows");

    host.settle();
    assert_eq!(host.quits, 1);
    assert!(
        parts.upgrade().is_some(),
        "the review went while a job held it"
    );
    drop(running);
    assert!(parts.upgrade().is_none(), "the review outlived its jobs");
}

/// Once the engine stops as the app quits, the driver takes no change, a click on a folder
/// included, while what the jobs hand back still lands, so the app quits.
#[test]
fn closing_takes_no_click_and_still_quits() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::Quit);
    while host.model().quitting != Quitting::Closing {
        let saved = host.run_job();
        host.complete(saved);
    }
    let folder = host
        .model()
        .tree
        .iter()
        .position(|node| node.kind == NodeKind::Directory)
        .expect("the demo has a folder");
    let clicked = host.try_update(|model| model.click_tree_row(folder, folder, 1));
    assert!(clicked.is_none(), "the click acts");
    assert!(host.model().tree_collapsed.is_empty(), "the folder folds");

    host.settle();
    assert_eq!(host.quits, 1, "the engine's stop does not land");
}

/// An eased scroll still on its way when the engine starts to stop ends there, since layout,
/// which moves it on, takes no turn any more: once the app quits, nothing keeps it busy.
#[test]
fn closing_ends_an_eased_scroll() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.dispatch(Action::HalfPageDown);
    assert!(host.model().diff_scroll.animating());

    host.dispatch(Action::Quit);
    host.settle();
    assert_eq!(host.quits, 1);
    assert!(!host.model().busy(), "the scroll keeps the app busy");
}

/// A grant made with no definition waiting for it says it was made, and goes to no definition.
#[test]
fn a_grant_with_no_definition_waiting_goes_to_none() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host_with(
        dir.path(),
        Settings {
            language_servers: false,
            ..settings()
        },
    );
    assert!(host.model().buffer.is_none());
    trust(&mut host);
    assert!(
        host.model().status.starts_with("Trusted this checkout"),
        "{}",
        host.model().status
    );
}

/// A capture that read the trust before a revocation was recorded never shows that reading:
/// it counts for nothing once the revocation began, and trust is read again for the capture,
/// the revocation showing meanwhile.
#[test]
fn a_capture_that_read_trust_before_a_revocation_never_shows_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    trust(&mut host);

    // A capture reads the grant, and the reviewer revokes it before the capture shows.
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "2")).unwrap();
    host.update(Model::changed);
    let capture = host.run_newest_job();
    host.dispatch(Action::ToggleTrust);
    let revoked = host.run_newest_job();
    host.complete(capture);
    host.complete(revoked);
    assert_eq!(host.model().trust, Trust::Denied);
    assert!(host.model().live.as_ref().unwrap().has_waiting());

    // The capture shows with the next frame, without its reading of the grant.
    host.draw();
    assert!(
        selected_text(host.model()).contains("uno"),
        "the capture shows"
    );
    assert_eq!(host.model().trust, Trust::Denied, "the revocation shows");
    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);
}

/// A revocation that cannot be recorded stops the language servers all the same, keeps the
/// window denied whatever is read later, since the grant stays stored, until the reviewer
/// grants trust again, and stops a quit the reviewer waits for, so its warning stays in view.
#[cfg(target_os = "linux")]
#[test]
fn a_revocation_that_fails_stops_the_servers_and_stays_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    trust(&mut host);
    let (held, process) = serve(&mut host, "rust", dir.path());
    drop(held);

    // The review's data cannot be written while the revocation is recorded and the reviewer
    // waits to quit.
    host.dispatch(Action::ToggleTrust);
    host.dispatch(Action::Quit);
    let data = data_dir(&host);
    set_writable(&data, false);
    host.settle();
    set_writable(&data, true);
    let model = host.model();
    assert_eq!(model.trust, Trust::Denied);
    assert_eq!(model.parts().servers.len(), 0);
    assert!(stopped(process), "the server still runs");
    assert!(host.review.trusted().unwrap(), "the grant is still stored");
    assert_eq!(host.quits, 0, "the failure keeps the app open");
    assert_eq!(model.quitting, Quitting::Stopped);
    assert!(
        model.status.starts_with("Could not revoke trust")
            && model.status.contains("the stored grant may still apply")
            && model
                .status
                .ends_with("Quitting stopped; quit again to leave anyway."),
        "{}",
        model.status
    );

    // A capture of another branch says nothing over the failure while the quit is stopped.
    git_in(&repo, &["checkout", "--quiet", "-b", "other"]);
    host.update(Model::changed);
    host.settle();
    assert!(
        host.model().status.starts_with("Could not revoke trust"),
        "{}",
        host.model().status
    );

    // Captures of the same code and of other code read the stored grant; the window stays
    // denied.
    host.update(Model::changed);
    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);

    // A grant made again counts.
    trust(&mut host);
}

/// Quitting waits for a decision on trust as for a mark, takes no new decision meanwhile, and
/// then waits for the engine to stop the language servers, after which nothing holds the
/// review.
#[cfg(target_os = "linux")]
#[test]
fn quitting_waits_for_trust_and_the_engine() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    let parts = host.model().weak_parts();
    trust(&mut host);
    let (held, process) = serve(&mut host, "rust", dir.path());
    drop(held);

    // A mark saves while the reviewer quits; no decision on trust is taken meanwhile.
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::Quit);
    host.dispatch(Action::ToggleTrust);
    assert_eq!(
        host.model().status,
        "Trust is not changed while quitting. Saving 1 change to the review before quitting…"
    );
    assert_eq!(host.model().trust, Trust::Granted);

    // Once everything is saved, the engine stops, and only then does the app quit.
    while host.model().quitting != Quitting::Closing {
        let saved = host.run_job();
        host.complete(saved);
    }
    assert_eq!(host.quits, 0);
    assert_eq!(host.model().status, "Closing the review before quitting…");
    assert!(parts.upgrade().is_some());
    host.settle();
    assert_eq!(host.quits, 1);
    assert!(stopped(process), "the server still runs");
    assert!(
        parts.upgrade().is_none(),
        "the engine still holds the review"
    );

    // A revocation under way is recorded before the app quits.
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    trust(&mut host);
    host.dispatch(Action::ToggleTrust);
    host.dispatch(Action::Quit);
    assert_eq!(
        host.model().status,
        "Saving 1 change to the review before quitting…"
    );
    host.settle();
    assert_eq!(host.quits, 1);
    assert!(!checkout_review(&host).trusted().unwrap());
}

/// Go to definition that asked for trust goes on once the grant is recorded, in the buffer it
/// asked in, unless another key came first.
#[test]
fn a_definition_waiting_for_trust_goes_on_once_granted() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    host.update(|model| model.show_file("a.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleBuffer);
    host.dispatch(Action::NextWord);
    let ask = |host: &mut Host| {
        host.dispatch(Action::GoToDefinition);
        // The question comes once the disk is found to hold what shows.
        let checked = host.run_newest_job();
        host.complete(checked);
        assert_eq!(host.model().overlay, Some(Overlay::Trust));
        host.dispatch(Action::TrustCheckout);
        assert_eq!(host.model().trust, Trust::Checking);
    };

    ask(&mut host);
    let granted = host.run_newest_job();
    host.complete(granted);
    assert!(host.model().looking_up, "{}", host.model().status);
    // The lookup would start a language server, so it never runs here.
    drop(host.jobs.pop_back());
    host.update(|model| model.looking_up = false);
    host.dispatch(Action::ToggleTrust);
    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);

    ask(&mut host);
    host.dispatch(Action::CursorDown);
    host.settle();
    assert!(host.model().trust.granted());
    assert!(!host.model().looking_up, "another key came first");
}

/// A capture cuts the way back from a definition into a buffer of the old snapshot, whose
/// change markers number the old snapshot's hunks.
#[test]
fn a_capture_cuts_the_way_back_to_an_old_review_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    trust(&mut host);
    host.dispatch(Action::ToggleBuffer);
    host.update(|model| model.looking_up = true);
    let asked = lookup(host.model());
    host.complete(found_for(asked));
    assert!(host.model().buffer.as_ref().unwrap().previous.is_some());

    edit_and_capture(&mut host, &repo, "uno", "two");
    let model = host.model();
    assert_eq!(model.mode, Mode::Buffer);
    let buffer = model.buffer.as_ref().unwrap();
    assert_eq!(buffer.title(), "src/ledger.rs");
    assert!(
        buffer.previous.is_none(),
        "no way back into the old snapshot"
    );
}

/// In a review buffer, the cursor stays on the hunk the keys acted on when lines above it come
/// and go, rather than on its old row.
#[test]
fn a_review_buffer_keeps_its_hunk_under_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleBuffer);
    assert_eq!(host.model().buffer_hunk(), Some(1));

    let more = ["one", "    a", "    b", "    c", "    d"].join("\n");
    edit_and_capture(&mut host, &repo, &more, "two");
    let model = host.model();
    assert_eq!(model.mode, Mode::Buffer);
    assert_eq!(model.buffer_hunk(), Some(model.hunk));
    assert!(selected_text(model).contains("two"));
}

/// A folder the tree cursor lands on because the one under it went takes the review keys as
/// soon as a frame shows the cursor there; before that frame, a key acts on what showed.
#[test]
fn a_folder_the_cursor_lands_on_takes_review_keys_once_shown() {
    let dir = tempfile::tempdir().unwrap();
    let (review, repo) = worktree_review_of(
        dir.path(),
        &[("a.rs", "fn a() {}\n")],
        &[("a.rs", "fn a() { 1 }\n")],
    );
    for (path, text) in [("x/one.rs", "fn one() {}\n"), ("y/two.rs", "fn two() {}\n")] {
        std::fs::create_dir_all(repo.join(path).parent().unwrap()).unwrap();
        std::fs::write(repo.join(path), text).unwrap();
    }
    review.refresh().unwrap();
    let mut host = Host::open(review);
    host.update(|model| model.show_file("a.rs"));
    let folder = host
        .model()
        .visible_tree()
        .iter()
        .position(|&index| host.model().tree[index].path == "x")
        .unwrap();
    host.update(|model| model.tree_cursor = folder);
    assert_eq!(host.model().tree_node().unwrap().path, "x");

    std::fs::remove_dir_all(repo.join("x")).unwrap();
    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(host.model().tree_node().unwrap().path, "y");
    host.dispatch(Action::ToggleApprove);
    assert!(
        host.model().status.starts_with("Approved y"),
        "{}",
        host.model().status
    );
}

/// A capture of the snapshot on screen with marks another window saved since shows them, and
/// so does `R`, which reads them first.
#[test]
fn marks_saved_elsewhere_show_with_the_next_capture() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    assert_eq!(host.model().file_totals(), (0, 1));
    let review = host.review.clone();
    let history = review.history().unwrap();
    let view = history.current_view().unwrap();
    review
        .mark(&view, &view.unit_ids(), MarkVerdict::Accepted)
        .unwrap();

    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(host.model().file_totals(), (1, 1));

    let history = review.history().unwrap();
    let view = history.current_view().unwrap();
    review
        .mark(&view, &view.unit_ids(), MarkVerdict::Unseen)
        .unwrap();
    host.dispatch(Action::Refresh);
    host.settle();
    assert_eq!(host.model().file_totals(), (0, 1), "R shows the reset");
    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(host.model().file_totals(), (0, 1));
}

/// A capture keeps the reviewer in the diff when they left the buffer for it.
#[test]
fn a_capture_does_not_reopen_a_buffer_left_for_the_diff() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    host.dispatch(Action::ToggleBuffer);
    host.dispatch(Action::ToggleBuffer);
    assert_eq!(host.model().mode, Mode::Diff);
    edit_and_capture(&mut host, &repo, "uno", "two");
    assert_eq!(host.model().mode, Mode::Diff);
}

/// The defaults with the whole file shown, as the apps open.
fn whole_file() -> Settings {
    let mut settings = settings();
    settings.appearance.applied.view = crate::config::DiffView::File;
    settings
}

/// The whole-file view with `[diff] cross_files` on.
fn crossing_files() -> Settings {
    let mut settings = whole_file();
    settings.appearance.applied.cross_files = true;
    settings
}

/// By default the hunk keys stay within the file: `n` on the last hunk and `]` past the last
/// hunk to review keep the selection and say which file is next.
#[test]
fn hunk_keys_stay_in_the_file_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    let engine = "src/engine.rs";
    host.update(|model| model.show_file(engine));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.dispatch(Action::LastHunk);
    let last = host.model().hunk;
    host.dispatch(Action::NextHunk);
    assert_eq!(host.model().selected.as_deref(), Some(engine));
    assert_eq!(host.model().hunk, last);
    assert_eq!(host.model().status, "This is the file's last hunk.");
    host.dispatch(Action::FirstHunk);
    host.dispatch(Action::PreviousHunk);
    assert_eq!(host.model().hunk, 0);
    assert_eq!(host.model().status, "This is the file's first hunk.");
    // The engine's only open hunk is its last; past it `]` stays and names the next file, and
    // once it is approved, says the file is done.
    host.dispatch(Action::NextOpenHunk);
    assert_eq!(host.model().hunk, 2);
    let next = host.model().next_open_file().unwrap();
    host.dispatch(Action::NextOpenHunk);
    assert_eq!(host.model().selected.as_deref(), Some(engine));
    assert_eq!(host.model().hunk, 2);
    assert_eq!(
        host.model().status,
        format!("This is the file's last hunk to review; {next} is next.")
    );
    host.dispatch(Action::ToggleApprove);
    assert_eq!(
        host.model().status,
        format!("This file is done; {next} is next to review.")
    );
    host.dispatch(Action::NextOpenHunk);
    assert_eq!(host.model().selected.as_deref(), Some(engine));
    assert_eq!(
        host.model().status,
        format!("This file is done; {next} is next to review.")
    );
}

/// With `[diff] cross_files`, `n` past a file's last hunk goes on to the next file in the tree
/// at its first hunk, and `N` comes back to the last hunk, reviewed or not.
#[test]
fn hunks_step_on_into_the_next_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), crossing_files());
    let engine = "src/engine.rs";
    host.update(|model| model.show_file(engine));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    assert!(
        host.model()
            .rows
            .rows
            .iter()
            .all(|row| row.kind() != rows::RowKind::Header),
        "the whole file shows without hunk headers"
    );
    host.dispatch(Action::FirstHunk);
    assert_eq!(host.model().hunk, 0);
    host.dispatch(Action::NextHunk);
    assert_eq!(host.model().hunk, 1, "an approved hunk is a stop too");
    host.dispatch(Action::LastHunk);
    let last = host.model().hunk;
    let next = host
        .model()
        .adjacent_file(true)
        .map(|position| path_at(host.model(), position))
        .unwrap();
    host.dispatch(Action::NextHunk);
    assert_eq!(host.model().selected.as_deref(), Some(next.as_str()));
    assert_eq!(host.model().rows.files[0].path, next);
    assert_eq!(host.model().hunk, 0);
    assert!(
        !host.model().diff_scroll.animating(),
        "another file shows at its hunk at once, without a glide from its last place"
    );
    host.dispatch(Action::PreviousHunk);
    assert_eq!(host.model().selected.as_deref(), Some(engine));
    assert_eq!(host.model().hunk, last);
}

/// `]` skips approved hunks and, with `[diff] cross_files`, goes on into the next file that
/// needs the reviewer, at its first such hunk, and `[` comes back.
#[test]
fn hunks_to_review_step_past_approved_ones_and_on_into_the_next_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), crossing_files());
    let engine = "src/engine.rs";
    host.update(|model| model.show_file(engine));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    // The demo accepted the first hunk and the reflow needs no mark, so the engine's only open
    // hunk is its last.
    host.dispatch(Action::FirstHunk);
    host.dispatch(Action::NextOpenHunk);
    let open = host.model().hunk;
    assert_eq!(open, 2);
    let next = host.model().next_open_file().unwrap();
    host.dispatch(Action::NextOpenHunk);
    assert_eq!(host.model().selected.as_deref(), Some(next.as_str()));
    let hunk = host.model().hunk;
    assert!(
        !hunk_badges(host.model(), hunk)
            .into_iter()
            .all(state::is_approved),
        "it lands on a hunk that needs the reviewer"
    );
    // Back before the next file's first open hunk is the engine's last one.
    while host.model().open_hunk_before(host.model().hunk).is_some() {
        host.dispatch(Action::PreviousOpenHunk);
    }
    host.dispatch(Action::PreviousOpenHunk);
    assert_eq!(host.model().selected.as_deref(), Some(engine));
    assert_eq!(host.model().hunk, open);
}

/// By default a hunk the keys go to shows with a fifth of the diff clear above it and, as far
/// as it fits, below it, landing a fifth down whichever way the keys go, so the code around it
/// shows and a long hunk shows from its start.
#[test]
fn hunks_land_a_fifth_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("server.go"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let viewport = host.model().grid().viewports.diff.unwrap();
    let margin = (viewport * scroll::READING_LINE).floor();
    let max = crate::frame::max_offset(host.model().rows.rows.len(), Some(viewport));
    let count = host.model().hunk_rows.len();
    for hunk in (0..count).chain((0..count).rev()) {
        host.update(|model| model.select_hunk(hunk));
        let model = host.model();
        let rows = &model.hunk_rows[hunk];
        let (start, end) = (scroll::rows(rows.start), scroll::rows(rows.end));
        let top = model.diff_scroll.target();
        assert!(
            start - top >= margin || top == 0.0,
            "hunk {hunk} keeps its margin above: {start} at {top}"
        );
        assert!(
            end - top <= viewport - margin || end - start > viewport - 2.0 * margin || top >= max,
            "hunk {hunk} keeps its margin below: {end} at {top}"
        );
    }
}

/// A file seen for the first time shows from its first change that needs the reviewer, at the
/// reading line, not from its first line.
#[test]
fn a_file_opens_at_its_first_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    lay_out(&mut host);
    host.update(|model| model.show_file("src/engine.rs"));
    lay_out(&mut host);
    let model = host.model();
    assert_eq!(model.hunk, 2, "the first hunk that needs the reviewer");
    let viewport = model.grid().viewports.diff.unwrap();
    let start = scroll::rows(model.hunk_rows[2].start);
    let top = model.diff_scroll.offset();
    let max = crate::frame::max_offset(model.rows.rows.len(), Some(viewport));
    assert!(top > 0.0, "not from the file's first line");
    assert!(
        (start - top - (viewport * scroll::READING_LINE).floor()).abs() < f64::EPSILON
            || (top - max).abs() < f64::EPSILON,
        "its start at the reading line: {start} at {top}"
    );
}

/// Lays the window out once every eased scroll has arrived.
fn settle_scroll(host: &mut Host) {
    host.update(|model| model.layout(fixed_window(), Instant::now() + scroll::DURATION));
}

/// The lines the diff's top row shows: its head and its base line.
fn top_line(model: &Model) -> (Option<usize>, Option<usize>) {
    let row = &model.rows.rows[scroll::whole_rows(model.diff_scroll.offset())];
    let cell = row.cells().next().expect("the top row shows code");
    (cell.head.as_ref().map(|head| head.line), cell.base)
}

/// `j` and `k` move between hunks, and in scroll mode, which `s` turns on and off, scroll the
/// diff a line instead, keeping the selection while it shows.
#[test]
fn scroll_mode_makes_the_hunk_keys_scroll() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.dispatch(Action::FirstHunk);
    settle_scroll(&mut host);
    host.dispatch(Action::NextHunk);
    assert_eq!(host.model().hunk, 1);
    settle_scroll(&mut host);
    host.dispatch(Action::ToggleScrollMode);
    assert!(host.model().scroll_mode);
    let top = host.model().diff_scroll.target();
    host.dispatch(Action::NextHunk);
    assert!((host.model().diff_scroll.target() - (top + 1.0)).abs() < f64::EPSILON);
    assert_eq!(host.model().hunk, 1, "the selection stays while it shows");
    host.dispatch(Action::PreviousHunk);
    assert!((host.model().diff_scroll.target() - top).abs() < f64::EPSILON);
    host.dispatch(Action::ToggleScrollMode);
    assert!(!host.model().scroll_mode);
    host.dispatch(Action::NextHunk);
    assert_eq!(host.model().hunk, 2);
}

/// Esc in scroll mode leaves it and goes back to the hunk it began at, however far the view
/// scrolled and whichever hunk the selection followed to; the next Esc goes to the files.
#[test]
fn escape_leaves_scroll_mode_at_its_hunk() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("scheduler.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.dispatch(Action::FirstHunk);
    host.dispatch(Action::NextHunk);
    settle_scroll(&mut host);
    host.dispatch(Action::ToggleScrollMode);
    for _ in 0..200 {
        host.dispatch(Action::NextHunk);
    }
    settle_scroll(&mut host);
    assert_ne!(host.model().hunk, 1, "the selection followed the view");
    host.dispatch(Action::FocusTree);
    assert!(!host.model().scroll_mode);
    assert_eq!(host.model().mode, Mode::Diff);
    assert_eq!(
        host.model().hunk,
        1,
        "back at the hunk scroll mode began at"
    );
    host.dispatch(Action::FocusTree);
    assert_eq!(host.model().mode, Mode::Tree);
}

/// Scrolling a line, and when the selected hunk leaves the view, the hunk on screen
/// nearest the reading line becomes the selected one, so Space marks what shows; however the
/// diff scrolls, the scrollbar's track included.
#[test]
fn the_selected_hunk_follows_the_view() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.dispatch(Action::FirstHunk);
    settle_scroll(&mut host);
    let top = host.model().diff_scroll.target();
    host.dispatch(Action::LineDown);
    assert!((host.model().diff_scroll.target() - (top + 1.0)).abs() < f64::EPSILON);
    host.dispatch(Action::LineUp);
    assert!((host.model().diff_scroll.target() - top).abs() < f64::EPSILON);
    settle_scroll(&mut host);
    let on_screen = |model: &Model, hunk: usize| {
        scroll::shows(
            &model.hunk_rows[hunk],
            model.diff_scroll.offset(),
            model.grid().viewports.diff,
        )
    };
    let mut moved = false;
    for _ in 0..host.model().rows.rows.len() {
        host.dispatch(Action::LineDown);
        settle_scroll(&mut host);
        let model = host.model();
        assert!(
            on_screen(model, model.hunk)
                || !(0..model.hunk_rows.len()).any(|hunk| on_screen(model, hunk)),
            "the selected hunk shows whenever any does"
        );
        moved |= model.hunk > 0;
    }
    assert!(moved, "scrolling down selects later hunks");

    // A click on the scrollbar's track pages too.
    host.dispatch(Action::FirstHunk);
    settle_scroll(&mut host);
    let before = host.model().diff_scroll.offset();
    host.update(|model| model.page_list_toward(List::Diff, 1000.0));
    // The first frame takes the page and starts its scroll, the next one sees it arrive.
    lay_out(&mut host);
    settle_scroll(&mut host);
    let model = host.model();
    assert!(model.diff_scroll.offset() > before, "the track paged");
    assert!(on_screen(model, model.hunk));
}

/// In the hunks view, an approved hunk folds the moment it is approved, while Space moves on to
/// the next hunk, so the save landing later, even while that scroll is on its way, moves no
/// row: the next hunk shows from its start and stays selected.
#[test]
fn a_saved_approval_keeps_the_line_at_the_top() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), marks_only());
    let short = Geometry {
        heights: Heights {
            diff: Some(8.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    let settle = |host: &mut Host| {
        host.update(|model| model.layout(short, Instant::now() + scroll::DURATION));
    };
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    settle(&mut host);
    // The reflow needs a mark here; approving it moves on to the last hunk.
    host.dispatch(Action::FirstHunk);
    host.dispatch(Action::NextHunk);
    settle(&mut host);
    assert_eq!(host.model().hunk, 1);
    host.dispatch(Action::ToggleApprove);
    assert_eq!(host.model().hunk, 2);
    let folded = |model: &Model| {
        model
            .rows
            .rows
            .iter()
            .any(|row| row.hunk == Some(1) && row.note().is_some_and(|note| note.folded))
    };
    assert!(folded(host.model()), "the approved reflow folds at once");
    let rows = host.model().rows.rows.len();
    // The save lands before the scroll to the next hunk arrives.
    let saved = host.run_job();
    host.complete(saved);
    assert_eq!(host.model().rows.rows.len(), rows, "the save moves no row");
    settle(&mut host);
    let model = host.model();
    assert!(folded(model));
    let start = scroll::rows(model.hunk_rows[2].start);
    let top = model.diff_scroll.offset();
    assert!(
        start >= top && start < top + 8.0,
        "the next hunk shows from its start"
    );
    assert_eq!(model.hunk, 2, "and stays selected");
}

/// While an eased scroll is still on its way, the selection follows only what is on screen,
/// never where the scroll is heading, so Space cannot mark a hunk before it shows.
#[test]
fn a_scroll_on_its_way_selects_nothing_unseen() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.dispatch(Action::FirstHunk);
    settle_scroll(&mut host);
    let first = host.model().hunk_units();
    // Half a page down shows the next hunk once the scroll arrives.
    // The frame is laid out at an instant from before the scroll started, so it is still on
    // its way however long the dispatch took.
    let before = Instant::now();
    host.dispatch(Action::HalfPageDown);
    host.update(|model| model.layout(fixed_window(), before));
    assert_eq!(host.model().hunk_units(), first, "nothing else showed yet");
    host.dispatch(Action::ToggleApprove);
    let marked: Vec<UnitId> = host.model().pending.keys().copied().collect();
    assert!(
        marked.iter().all(|unit| first.contains(unit)),
        "only the hunk that showed is marked, or nothing: {marked:?}"
    );
}

/// Switching between unified and side by side keeps the line at the top, though a line edited in
/// place shows once in one layout and as two lines in the other.
#[test]
fn switching_the_layout_keeps_the_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    let short = Geometry {
        heights: Heights {
            diff: Some(8.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    let settle = |host: &mut Host| {
        host.update(|model| model.layout(short, Instant::now() + scroll::DURATION));
    };
    // The head line the top row shows, on either side of a pair.
    let top_head = |model: &Model| {
        let row = &model.rows.rows[scroll::whole_rows(model.diff_scroll.offset())];
        row.cells()
            .find_map(|cell| cell.head.as_ref().map(|head| head.line))
    };
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    settle(&mut host);
    // A head line past the first hunk, which shows once unified, at the top.
    let past = host.model().hunk_rows[0].end;
    let start = (past..host.model().rows.rows.len())
        .find(|&row| {
            host.model().rows.rows[row]
                .cells()
                .any(|cell| cell.head.is_some())
        })
        .unwrap();
    host.update(|model| model.drag_list_to(List::Diff, scroll::rows(start)));
    settle(&mut host);
    let line = top_head(host.model());
    assert!(line.is_some(), "a head line at the top");
    host.dispatch(Action::ToggleLayout);
    settle(&mut host);
    assert_eq!(
        host.model().presented.layout,
        crate::config::DiffLayout::SideBySide
    );
    assert_eq!(top_head(host.model()), line, "side by side keeps the line");
    host.dispatch(Action::ToggleLayout);
    settle(&mut host);
    assert_eq!(top_head(host.model()), line, "unified keeps it too");
}

/// Switching between the whole file and hunks apart keeps the line at the top, and a file left
/// in one view comes back at its line in the other.
#[test]
fn the_view_keeps_the_place() {
    let dir = tempfile::tempdir().unwrap();
    // Approved hunks show in the hunks view too, so the first hunk's code shows in both.
    let settings = Settings {
        approved: crate::config::Approved::Shown,
        ..whole_file()
    };
    let mut host = Host::open_with(demo(dir.path()), settings);
    // A diff eight rows tall, so neither view of the engine fits it and each can put any of its
    // lines at the top.
    let short = Geometry {
        heights: Heights {
            diff: Some(8.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    let settle = |host: &mut Host| {
        host.update(|model| model.layout(short, Instant::now() + scroll::DURATION));
    };
    let engine = "src/engine.rs";
    host.update(|model| model.show_file(engine));
    host.dispatch(Action::FocusDiff);
    settle(&mut host);
    // The first hunk's first line at the top: a changed line, which both views show.
    let start = host.model().hunk_rows[0].start;
    host.update(|model| model.drag_list_to(List::Diff, scroll::rows(start)));
    settle(&mut host);
    let line = top_line(host.model());
    assert_eq!(
        line,
        (Some(7), None),
        "the first hunk's line, edited in place"
    );
    host.dispatch(Action::ToggleView);
    settle(&mut host);
    assert_eq!(host.model().presented.view, crate::config::DiffView::Hunks);
    assert_eq!(top_line(host.model()), line, "hunks apart keep the line");
    host.dispatch(Action::ToggleView);
    settle(&mut host);
    assert_eq!(top_line(host.model()), line, "the whole file keeps it too");

    // Left in one view, the engine comes back at its line in the other, either way.
    for _ in 0..2 {
        host.update(|model| model.show_file("src/format.rs"));
        host.dispatch(Action::ToggleView);
        settle(&mut host);
        host.update(|model| model.show_file(engine));
        settle(&mut host);
        assert_eq!(top_line(host.model()), line);
    }
}

/// With `[diff] hunk_position = "center"`, every hunk the keys go to lands in the middle of the
/// diff, and `z z` centers the selected hunk once in either position.
#[test]
fn hunks_can_land_in_the_middle() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = whole_file();
    settings.appearance.applied.hunk_position = crate::config::HunkPosition::Center;
    let mut host = Host::open_with(demo(dir.path()), settings);
    host.update(|model| model.show_file("scripts/install.sh"));
    host.dispatch(Action::FocusDiff);
    settle_scroll(&mut host);
    let centered = |model: &Model| {
        let rows = &model.hunk_rows[model.hunk];
        let viewport = model.grid().viewports.diff.unwrap();
        let expected = scroll::centered(
            viewport,
            scroll::rows(rows.start),
            scroll::rows(rows.end),
            scroll::READING_LINE,
        );
        let max = crate::frame::max_offset(model.rows.rows.len(), Some(viewport));
        (model.diff_scroll.offset() - expected.min(max)).abs() < f64::EPSILON
    };
    host.dispatch(Action::FirstHunk);
    settle_scroll(&mut host);
    for _ in 1..host.model().hunk_rows.len() {
        host.dispatch(Action::NextHunk);
        settle_scroll(&mut host);
        assert!(centered(host.model()), "hunk {}", host.model().hunk);
    }

    // With margins, `z z` still centers once.
    host.update(|model| {
        model.settings.appearance.applied.hunk_position = crate::config::HunkPosition::Margin;
    });
    host.dispatch(Action::FirstHunk);
    settle_scroll(&mut host);
    host.dispatch(Action::LineDown);
    host.dispatch(Action::LineDown);
    settle_scroll(&mut host);
    host.dispatch(Action::CenterHunk);
    settle_scroll(&mut host);
    assert!(centered(host.model()));
}

/// The unit of `install.sh` a few lines below the changes above it: its last change, the
/// checksum download.
fn joined_unit(model: &Model) -> UnitId {
    let view = model.view().unwrap();
    view.units_of_file("scripts/install.sh")
        .into_iter()
        .find(|&unit| {
            view.unit(unit)
                .ok()
                .and_then(|record| {
                    record
                        .chunk()
                        .map(|chunk| chunk.added.contains("checksums"))
                })
                .unwrap_or(false)
        })
        .unwrap()
}

/// A buffer's change markers name units, so after the view changes, the hunk under the buffer's
/// cursor is still the one holding the line's change.
#[test]
fn the_buffer_marks_the_unit_under_its_cursor_whatever_the_view() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("scripts/install.sh"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let unit = joined_unit(host.model());
    let hunk = host.model().hunk_of(unit).unwrap();
    host.update(|model| model.select_hunk(hunk));
    host.dispatch(Action::ToggleBuffer);
    assert_eq!(host.model().buffer_hunk(), Some(hunk));
    host.update(|model| model.apply_view(crate::config::DiffView::Hunks));
    let shown = host.model().buffer_hunk().unwrap();
    assert_eq!(host.model().rows.hunks[shown].unit, unit);
}

/// A file left with a hunk selected comes back to the hunk holding the same change in the other
/// view.
#[test]
fn a_file_comes_back_at_its_change_in_the_other_view() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    let install = "scripts/install.sh";
    host.update(|model| model.show_file(install));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let unit = joined_unit(host.model());
    let hunk = host.model().hunk_of(unit).unwrap();
    host.update(|model| model.select_hunk(hunk));
    host.update(|model| model.show_file("scripts/release.sh"));
    host.dispatch(Action::ToggleView);
    host.update(|model| model.show_file(install));
    assert_eq!(host.model().hunk_units(), [unit]);
    // And back.
    host.update(|model| model.show_file("scripts/release.sh"));
    host.dispatch(Action::ToggleView);
    host.update(|model| model.show_file(install));
    assert_eq!(host.model().hunk_units(), [unit]);
}

/// A file left with a later hunk selected comes back to it after a capture of an edit to another
/// file: the place it kept follows its unit into the captured snapshot.
#[test]
fn a_capture_keeps_the_places_of_other_files() {
    let dir = tempfile::tempdir().unwrap();
    let (review, repo) = worktree_review_of(
        dir.path(),
        &[
            ("a.rs", &two_hunks("1", "2")),
            ("b.rs", &two_hunks("3", "4")),
        ],
        &[
            ("a.rs", &two_hunks("one", "two")),
            ("b.rs", &two_hunks("three", "four")),
        ],
    );
    let mut host = Host::open(review);
    host.update(|model| model.show_file("a.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::LastHunk);
    assert!(selected_text(host.model()).contains("two"));
    host.update(|model| model.show_file("b.rs"));

    std::fs::write(repo.join("b.rs"), two_hunks("three", "fourth")).unwrap();
    host.update(|model| model.capture(false));
    host.settle();
    assert!(host.model().view().unwrap().is_provisional());

    host.update(|model| model.show_file("a.rs"));
    assert!(
        selected_text(host.model()).contains("two"),
        "a.rs comes back at the hunk it was left at"
    );
}

/// Runs git in `repo`, as a reviewer does in their terminal.
fn git_in(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .envs(crate::testing::GIT_ENV)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// The review of a checkout in `dir`, on a clean `main` with `a.rs`, which `origin/main` is
/// level with, opened as the model's opener opens it; the repository has no remote to look a
/// pull request up on.
/// Returns it with the working tree.
fn open_checkout(dir: &Path) -> (Loaded, std::path::PathBuf) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(repo.join("a.rs"), two_hunks("1", "2")).unwrap();
    git_in(&repo, &["add", "--all"]);
    git_in(&repo, &["commit", "--quiet", "-m", "base"]);
    git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let loaded = crate::launch::open_checkout(
        &dir.join("data"),
        review_core::Repo::discover(&repo).unwrap(),
    )
    .unwrap();
    (loaded, repo)
}

/// The window over the review of a checkout in `dir`, as [`open_checkout`] opens it, opened and
/// settled.
/// Returns it with the working tree.
fn checkout_host(dir: &Path) -> (Host, std::path::PathBuf) {
    let (loaded, repo) = open_checkout(dir);
    let host = Host::open_loaded(loaded, settings());
    assert!(host.model().following(), "the checkout's review follows it");
    (host, repo)
}

/// An edit made after the opening capture, while the review was still on its way to the
/// screen, shows with the first capture once the watch starts.
#[test]
fn an_edit_made_while_the_review_opens_shows() {
    let dir = tempfile::tempdir().unwrap();
    let (loaded, repo) = open_checkout(dir.path());
    let mut host = Host::opening(loaded, settings());
    let opened = host.run_job();
    host.complete(opened);
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();

    host.draw();
    assert_eq!(
        file_paths(&host.model().tree),
        Vec::<&str>::new(),
        "the review shows as the opening captured it"
    );
    host.settle();
    assert_eq!(file_paths(&host.model().tree), ["a.rs"]);
}

/// Quitting while the review opens quits at once, and the review that opens after it never
/// shows: its engine goes as it lands, letting go of the review.
#[cfg(target_os = "linux")]
#[test]
fn a_review_opened_after_quitting_never_shows() {
    let dir = tempfile::tempdir().unwrap();
    let (loaded, _repo) = open_checkout(dir.path());
    let mut host = Host::opening(loaded, settings());
    host.dispatch(Action::Quit);
    assert_eq!(host.quits, 1, "nothing waits for the opening");

    let opened = host.run_job();
    let Outcome::Opened(Ok(shown)) = &*opened.outcome else {
        panic!("the review opens");
    };
    let parts = shown.engine.weak();
    host.complete(opened);
    let model = host.model();
    assert!(matches!(model.phase, Phase::Loading), "nothing shows");
    assert!(!model.publishes(), "nothing waits for a frame");
    assert!(!model.following());
    assert!(host.jobs.is_empty(), "nothing starts");
    assert!(parts.upgrade().is_none(), "the engine outlived the review");
}

/// The files pane's header, as review-tui shows it.
fn header(model: &Model) -> String {
    let header = crate::paint::files::files_header(model, &crate::paint::Palette::of(model));
    match header.number {
        Some(number) => format!("{} {}", number.text, header.name.text),
        None => header.name.text,
    }
}

/// The diff's card, when it shows one.
fn card(model: &Model) -> Option<(String, String)> {
    crate::paint::header::diff_state(model, &crate::paint::Palette::of(model))
        .map(|card| (card.title.text, card.detail.text))
}

/// The bottom bar's left side, as one line.
fn bar(model: &Model) -> String {
    crate::paint::bars::sync_label(
        model,
        &crate::paint::Palette::of(model),
        std::time::SystemTime::now(),
        f32::MAX,
        |text| scroll::float(text.chars().count()),
    )
    .into_iter()
    .map(|piece| piece.text)
    .collect()
}

/// A clean default branch opens empty and says why; an edit shows at once, without a restart.
#[test]
fn a_clean_checkout_opens_empty_and_shows_an_edit() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    let model = host.model();
    assert_eq!(file_paths(&model.tree), Vec::<&str>::new());
    assert_eq!(header(model), "main → origin/main");
    assert_eq!(
        card(model),
        Some((
            "Nothing to review".to_owned(),
            "main is level with origin/main. Nothing to review; changes appear here as you \
             make them."
                .to_owned()
        ))
    );
    assert_eq!(model.title(), "repo · main");
    assert!(
        bar(model).ends_with(" · live · untrusted"),
        "{}",
        bar(model)
    );
    assert!(
        model.status.starts_with("Following main → origin/main"),
        "{}",
        model.status
    );

    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    let model = host.model();
    assert_eq!(file_paths(&model.tree), ["a.rs"]);
    assert!(
        model.view().unwrap().is_provisional(),
        "nothing is recorded"
    );
    assert_eq!(card(model), None);
}

/// A branch made from the checkout, then renamed, shows in the header and the title, and is
/// announced, while the files stay as they are: the code did not change.
#[test]
fn a_new_branch_is_announced_and_moves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    let shown = host.model().shown;

    git_in(&repo, &["checkout", "--quiet", "-b", "feat"]);
    host.update(Model::changed);
    host.settle();
    let model = host.model();
    assert_eq!(model.shown, shown, "the same snapshot stays");
    assert_eq!(header(model), "feat → origin/main");
    assert_eq!(model.title(), "repo · feat");
    assert_eq!(model.status, "Now on feat.");

    git_in(&repo, &["branch", "-m", "feat/renamed"]);
    host.update(Model::changed);
    host.settle();
    assert_eq!(header(host.model()), "feat/renamed → origin/main");
    assert_eq!(host.model().status, "Now on feat/renamed.");
}

/// A capture taken before the watch saw another change never shows: the view on screen stays,
/// as updating, and a mark on it is queued, for the engine to check against what showed.
#[test]
fn a_capture_older_than_a_change_never_shows() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    host.update(|model| model.show_file("a.rs"));
    host.dispatch(Action::FocusDiff);
    let shown = host.model().shown;

    std::fs::write(repo.join("a.rs"), two_hunks("uno", "2")).unwrap();
    host.update(Model::changed);
    let first = host.run_newest_job();
    // The watch sees another change while the first capture runs.
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "dos")).unwrap();
    host.update(Model::changed);
    assert!(host.model().live.as_ref().unwrap().updating());
    host.complete(first);
    assert_eq!(host.model().shown, shown, "the older capture does not show");
    assert!(bar(host.model()).contains("updating…"));
    host.dispatch(Action::ToggleApprove);
    assert!(
        !host.model().status.starts_with("Not marked"),
        "{}",
        host.model().status
    );
    assert!(!host.model().pending.is_empty(), "queued");

    host.settle();
    let model = host.model();
    assert_eq!(model.shown, shown + 1, "the newest capture shows");
    assert!(!model.live.as_ref().unwrap().updating());
    assert!(
        model
            .view()
            .unwrap()
            .unit_ids()
            .iter()
            .filter_map(|&unit| model.view().unwrap().unit(unit).ok()?.chunk().cloned())
            .any(|chunk| chunk.added.contains("dos"))
    );
}

/// While git is busy in the checkout, the view stays and still takes marks, which mark what it
/// shows; once git is done, the checkout is captured again.
#[test]
fn a_busy_checkout_keeps_the_view_and_takes_marks() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    host.update(|model| model.show_file("a.rs"));
    host.dispatch(Action::FocusDiff);
    let shown = host.model().shown;

    let merge_head = repo.join(".git/MERGE_HEAD");
    std::fs::write(&merge_head, "0000000000000000000000000000000000000000\n").unwrap();
    std::fs::write(repo.join("a.rs"), two_hunks("conflict", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    let model = host.model();
    assert_eq!(model.shown, shown, "the view stays");
    assert!(bar(model).contains("git: merging"), "{}", bar(model));
    host.dispatch(Action::ToggleApprove);
    assert!(!host.model().pending.is_empty(), "the mark is queued");
    host.settle();
    assert!(host.model().pending.is_empty(), "and saved");

    std::fs::remove_file(&merge_head).unwrap();
    host.update(Model::changed);
    host.settle();
    let model = host.model();
    assert!(model.shown > shown);
    assert!(model.live.as_ref().unwrap().busy().is_none());
    assert!(bar(model).contains("live"));
}

/// A checkout replaced by another clone stops being captured and says to reopen.
#[test]
fn a_replaced_checkout_stops_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    host.update(Model::changed);
    host.settle();
    let model = host.model();
    assert_eq!(model.replaced(), Some(Replaced::Clone));
    assert!(model.status.starts_with("This checkout was replaced"));
    assert_eq!(
        card(model).map(|(title, _)| title).as_deref(),
        Some("This checkout was replaced")
    );
    host.update(Model::changed);
    assert!(host.jobs.is_empty(), "nothing is captured any more");
}

/// The event log of the review of the checkout in `dir`, as [`open_checkout`] opens it.
fn log_of(dir: &Path) -> std::path::PathBuf {
    let reviews = dir.join("data").join("reviews");
    let review = std::fs::read_dir(reviews).unwrap().next().unwrap().unwrap();
    review.path().join("events.jsonl")
}

/// The window over the review of a checkout in `dir` whose top hunk of `a.rs` is approved and
/// saved, with the bottom one selected.
fn approved_checkout_host(dir: &Path) -> Host {
    let (mut host, repo) = checkout_host(dir);
    std::fs::write(repo.join("a.rs"), two_hunks("one", "two")).unwrap();
    host.update(Model::changed);
    host.settle();
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert!(log_of(dir).exists(), "the approval recorded the capture");
    host
}

/// A review whose event log was replaced, here by a shorter one, stops being captured, says
/// to reopen, and marks nothing more.
#[test]
fn a_replaced_log_stops_the_review_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = approved_checkout_host(dir.path());
    let log = log_of(dir.path());
    let text = std::fs::read_to_string(&log).unwrap();
    let first = text.lines().next().unwrap();
    std::fs::write(&log, format!("{first}\n")).unwrap();

    host.update(Model::changed);
    host.settle();
    let model = host.model();
    assert_eq!(model.replaced(), Some(Replaced::Log));
    assert!(
        model
            .status
            .starts_with("This review's event log was replaced")
    );
    assert!(bar(model).contains("log replaced"));
    host.update(Model::changed);
    assert!(host.jobs.is_empty(), "nothing is captured any more");
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert!(
        host.model()
            .status
            .starts_with("Not marked: this review's event log was replaced")
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("{first}\n"));
}

/// A mark made after the event log was replaced, before any capture saw it, writes nothing,
/// stops the review, and drops the marks still queued.
#[test]
fn a_mark_on_a_replaced_log_writes_nothing_and_stops_the_review() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = approved_checkout_host(dir.path());
    let log = log_of(dir.path());
    let text = std::fs::read_to_string(&log).unwrap();
    let first = text.lines().next().unwrap();
    std::fs::write(&log, format!("{first}\n")).unwrap();

    host.dispatch(Action::ToggleApprove);
    assert!(!host.model().pending.is_empty(), "the mark shows at once");
    host.settle();
    let model = host.model();
    assert_eq!(model.replaced(), Some(Replaced::Log));
    assert!(
        model
            .status
            .starts_with("This review's event log was replaced")
    );
    assert!(model.pending.is_empty() && model.saves.is_empty());
    assert!(
        matches!(
            *model.parts().watch.lock(),
            super::engine::WatchSlot::Closed
        ),
        "the working tree is not watched any more"
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("{first}\n"));
}

/// A quit that waits for the one mark being saved, which the event log replaced meanwhile
/// refuses, stops and says the mark was lost, which the app's log keeps as well.
#[test]
fn a_mark_a_replaced_log_refused_stops_the_quit_that_waited_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = approved_checkout_host(dir.path());
    let log = log_of(dir.path());
    let text = std::fs::read_to_string(&log).unwrap();
    let first = text.lines().next().unwrap();
    std::fs::write(&log, format!("{first}\n")).unwrap();

    host.dispatch(Action::ToggleApprove);
    assert!(host.model().saves.is_empty(), "the mark is being saved");
    host.dispatch(Action::Quit);
    host.settle();
    let model = host.model();
    assert_eq!(host.quits, 0, "the app does not close over the lost mark");
    assert_eq!(model.quitting, Quitting::Stopped);
    assert!(model.status.contains("Marks not saved yet are lost."));
    assert!(
        host.logged
            .iter()
            .any(|line| line.starts_with("could not record the mark of 1 unit"))
    );
}

/// The pull request's lookup: an answer for another branch is dropped, the branch's lands with
/// news and a capture that may move the base, and its merge is news too; a failure keeps what
/// was found, as stale.
#[test]
fn the_pull_request_follows_the_branch() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    git_in(&repo, &["checkout", "--quiet", "-b", "feat"]);
    host.update(Model::changed);
    host.settle();
    let association = host
        .model()
        .live
        .as_ref()
        .unwrap()
        .pulls
        .association()
        .cloned()
        .unwrap();
    assert_eq!(association.branch, "feat");
    let pull_request = |state| review_core::github::BranchPullRequest {
        number: 12,
        url: "https://github.com/owner/repo/pull/12".to_owned(),
        state,
        base_branch: "main".to_owned(),
        base_repository: review_core::github::Repository {
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        },
        head_branch: "feat".to_owned(),
        head_repository: None,
        author: None,
    };
    let looked = |looked: super::pulls::Looked| Completion {
        subject: Subject::Always(super::effects::Because::Live),
        outcome: Box::new(Outcome::PullRequest(Box::new(looked))),
    };
    let open = review_core::github::PullRequestState::Open;

    let other = review_core::BranchIdentity {
        branch: "other".to_owned(),
        remote: None,
        merge: None,
        push_remote: None,
        upstream: None,
    };
    host.complete(looked(super::pulls::Looked::found(
        other,
        Some(pull_request(open)),
    )));
    assert_eq!(header(host.model()), "feat → origin/main", "dropped");

    host.complete(looked(super::pulls::Looked::found(
        association.clone(),
        Some(pull_request(open)),
    )));
    assert_eq!(host.model().status, "feat has pull request #12 (open).");
    assert!(
        host.model().live.as_ref().unwrap().updating(),
        "a capture with the pull request follows"
    );
    host.settle();
    let model = host.model();
    assert!(!model.live.as_ref().unwrap().updating());
    assert_eq!(header(model), "#12 feat → origin/main");
    assert_eq!(model.title(), "repo · feat #12");
    // The window's title follows the pull request once the capture that used it shows.
    assert_eq!(
        host.titles.last().map(String::as_str),
        Some("repo · feat #12")
    );

    host.complete(looked(super::pulls::Looked::failed(association.clone())));
    let state = host.model().live_state().unwrap();
    assert!(state.pull_request.unwrap().stale.is_some());
    assert_eq!(header(host.model()), "#12 feat → origin/main", "kept");

    host.complete(looked(super::pulls::Looked::found(
        association,
        Some(pull_request(review_core::github::PullRequestState::Merged)),
    )));
    assert_eq!(host.model().status, "#12 merged.");
    host.settle();
    assert_eq!(header(host.model()), "#12 merged · feat → origin/main");
}

/// A capture that lands only after the notice of a write it overlapped still takes marks on
/// what it showed: the watch says so at the burst's first change, before it settles, and a
/// capture that lands while the burst is under way does not show, but a press on the one on
/// screen is queued, not refused.
#[test]
fn a_change_under_way_still_takes_marks_on_the_capture_it_overlapped() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    host.update(|model| model.show_file("a.rs"));
    host.dispatch(Action::FocusDiff);
    let shown = host.model().shown;

    // A capture completes and shows; the notice of a write it overlapped arrives after it.
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "2")).unwrap();
    host.update(Model::changed);
    let capture = host.run_newest_job();
    host.complete(capture);
    host.draw();
    assert_eq!(host.model().shown, shown + 1);
    std::fs::write(repo.join("a.rs"), two_hunks("uno", "dos")).unwrap();
    host.update(Model::stirred);
    host.dispatch(Action::ToggleApprove);
    assert!(
        !host.model().status.starts_with("Not marked"),
        "{}",
        host.model().status
    );
    assert!(!host.model().pending.is_empty(), "queued");

    // A capture taken while the burst is under way does not show either.
    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(host.model().shown, shown + 1, "nothing shows mid-burst");
    assert!(host.model().live.as_ref().unwrap().updating());

    // Once the burst settles, its capture shows and takes marks.
    host.update(Model::changed);
    host.settle();
    let model = host.model();
    assert_eq!(model.shown, shown + 2);
    assert!(!model.live.as_ref().unwrap().updating());
}

/// A capture resolves its base with the pull request found for the branch it sees checked out,
/// whichever branch the lookups are about when it is taken, and the header shows the pull
/// request that capture used.
#[test]
fn a_capture_takes_the_pull_request_of_the_branch_it_sees() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    git_in(&repo, &["checkout", "--quiet", "-b", "feat"]);
    host.update(Model::changed);
    host.settle();
    let feat = host
        .model()
        .live
        .as_ref()
        .unwrap()
        .pulls
        .association()
        .cloned()
        .unwrap();
    let pull_request = review_core::github::BranchPullRequest {
        number: 12,
        url: "https://github.com/owner/repo/pull/12".to_owned(),
        state: review_core::github::PullRequestState::Open,
        base_branch: "main".to_owned(),
        base_repository: review_core::github::Repository {
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        },
        head_branch: "feat".to_owned(),
        head_repository: None,
        author: None,
    };
    host.complete(Completion {
        subject: Subject::Always(super::effects::Because::Live),
        outcome: Box::new(Outcome::PullRequest(Box::new(super::pulls::Looked::found(
            feat,
            Some(pull_request),
        )))),
    });
    host.settle();
    assert_eq!(header(host.model()), "#12 feat → origin/main");

    // Off to another branch, which has no pull request found, and back.
    git_in(&repo, &["checkout", "--quiet", "-b", "other"]);
    host.update(Model::changed);
    host.settle();
    assert_eq!(header(host.model()), "other → origin/main");
    git_in(&repo, &["checkout", "--quiet", "feat"]);
    host.update(Model::changed);
    let capture = host.run_newest_job();
    host.complete(capture);
    host.draw();
    let model = host.model();
    let subject = model.live_state().unwrap().subject.unwrap();
    assert_eq!(
        subject.pull_request.as_ref().map(|found| found.number),
        Some(12),
        "the first capture back on feat has its pull request"
    );
    assert_eq!(header(model), "#12 feat → origin/main");
    assert_eq!(model.title(), "repo · feat #12");
}

/// A pull request found while a capture of its branch runs, which was taken without it, sends
/// a capture again, though nothing about the pull request is news by the time it shows.
#[test]
fn a_pull_request_found_during_a_capture_captures_again() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    git_in(&repo, &["checkout", "--quiet", "-b", "feat"]);
    // The switch's capture runs, given no pull request.
    host.update(Model::changed);
    let capture = host.run_newest_job();
    let feat = host.review.repo().branch_identity("feat").unwrap();
    let pull_request = review_core::github::BranchPullRequest {
        number: 12,
        url: "https://github.com/owner/repo/pull/12".to_owned(),
        state: review_core::github::PullRequestState::Open,
        base_branch: "main".to_owned(),
        base_repository: review_core::github::Repository {
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        },
        head_branch: "feat".to_owned(),
        head_repository: None,
        author: None,
    };
    // A lookup answers for feat while the window still follows main.
    host.complete(Completion {
        subject: Subject::Always(super::effects::Because::Live),
        outcome: Box::new(Outcome::PullRequest(Box::new(super::pulls::Looked::found(
            feat,
            Some(pull_request),
        )))),
    });
    host.complete(capture);
    host.settle();
    assert_eq!(header(host.model()), "#12 feat → origin/main");
}

/// Where no lookup of the pull request can run, it runs again once the branch moves, or after
/// the longest wait, without `r`.
#[test]
fn an_unavailable_lookup_is_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    git_in(&repo, &["checkout", "--quiet", "-b", "feat"]);
    host.update(Model::changed);
    host.settle();
    let feat = host
        .model()
        .live
        .as_ref()
        .unwrap()
        .pulls
        .association()
        .cloned()
        .unwrap();
    let looked = |looked: super::pulls::Looked| Completion {
        subject: Subject::Always(super::effects::Because::Live),
        outcome: Box::new(Outcome::PullRequest(Box::new(looked))),
    };
    host.complete(looked(super::pulls::Looked::unavailable(feat)));
    let state = host.model().live_state().unwrap();
    assert!(state.lookup.is_some());
    assert!(!state.looking_up);

    // Not before its time.
    host.update(Model::look_up_pull_request);
    assert!(!host.model().live_state().unwrap().looking_up);
    // Once due, as after `gh auth login`.
    host.update(|model| model.live.as_mut().unwrap().pulls.make_due());
    host.update(Model::pull_request_due);
    assert!(host.model().live_state().unwrap().looking_up);
    host.settle();
    assert!(host.model().live_state().unwrap().lookup.is_some());

    // On another branch, at once.
    git_in(&repo, &["checkout", "--quiet", "-b", "other"]);
    host.update(Model::changed);
    let capture = host.run_newest_job();
    host.complete(capture);
    host.draw();
    let state = host.model().live_state().unwrap();
    assert!(state.looking_up);
    assert!(state.lookup.is_none());
}

/// The checkout is never trusted by default: the first tool asks, and the grant covers the
/// code the checkout holds after later edits, until it is revoked.
#[test]
fn trust_in_the_checkout_is_asked_for_and_lasts() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);

    host.dispatch(Action::ToggleTrust);
    assert_eq!(host.model().overlay, Some(Overlay::Trust));
    // A capture read while the question shows waits behind it, with the trust it read then,
    // which no longer counts once the grant is made.
    host.update(Model::changed);
    let capture = host.run_newest_job();
    host.complete(capture);
    assert!(host.model().live.as_ref().unwrap().has_waiting());
    host.dispatch(Action::TrustCheckout);
    assert_eq!(host.model().trust, Trust::Checking);
    assert!(bar(host.model()).ends_with("checking trust"));
    host.settle();
    assert!(!host.model().live.as_ref().unwrap().has_waiting());
    let model = host.model();
    assert_eq!(model.overlay, None);
    assert_eq!(model.trust, Trust::Granted);
    assert!(bar(model).ends_with(" · trusted"), "{}", bar(model));

    std::fs::write(repo.join("a.rs"), two_hunks("uno", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    assert_eq!(host.model().trust, Trust::Granted, "it lasts");

    host.dispatch(Action::ToggleTrust);
    assert_eq!(host.model().status, "Revoking trust…");
    host.settle();
    assert_eq!(host.model().trust, Trust::Denied);
    assert!(
        host.model()
            .status
            .starts_with("Trust in this checkout revoked")
    );
}

/// A lookup on a view the disk has moved past, as before the capture of an edit shows, is
/// refused before any language server is asked or started.
#[test]
fn a_lookup_on_a_view_the_disk_moved_past_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    trust(&mut host);
    buffer_on_a_name(&mut host);
    std::fs::write(repo.join("b.rs"), "fn b() {}\n").unwrap();

    host.dispatch(Action::GoToDefinition);
    host.settle();
    assert_eq!(
        host.model().status,
        "Go to definition needs the code on disk, which no longer matches what shows; press r, \
         then g d again."
    );
    assert_eq!(host.model().parts().servers.len(), 0);
}

/// A revision older than the code on disk, as a review that does not follow the working tree
/// shows once `R` recorded a newer one, stops the language servers and refuses go to
/// definition, until `R` shows the newest; the grant stays.
#[cfg(target_os = "linux")]
#[test]
fn an_older_revision_stops_the_servers_and_refuses_lookups() {
    let dir = tempfile::tempdir().unwrap();
    let (review, repo) = worktree_review_of(
        dir.path(),
        &[("a.rs", &two_hunks("1", "2"))],
        &[("a.rs", &two_hunks("one", "two"))],
    );
    let mut host = Host::open_with(
        review,
        Settings {
            live: false,
            ..settings()
        },
    );
    trust(&mut host);
    let (held, process) = serve(&mut host, "rust-analyzer", dir.path());
    drop(held);

    std::fs::write(repo.join("a.rs"), two_hunks("uno", "two")).unwrap();
    host.dispatch(Action::Refresh);
    host.settle();
    assert_eq!(
        host.model().status,
        "Revision 2 recorded; press R to switch to it."
    );
    assert_eq!(host.model().parts().servers.len(), 0);
    assert!(stopped(process), "the server runs for an older revision");
    assert_eq!(host.model().trust, Trust::Granted);

    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    assert_eq!(
        host.model().status,
        "Go to definition needs the code on disk, which revision 2 holds; press R to switch to \
         it."
    );
    assert!(!host.model().looking_up, "a lookup runs");

    host.dispatch(Action::Refresh);
    host.settle();
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    assert!(host.model().looking_up, "{}", host.model().status);
}

/// A checkout replaced by another clone stops the language servers in the capture that finds
/// it, no grant applies to the new clone's code, and go to definition says to reopen the review.
#[cfg(target_os = "linux")]
#[test]
fn a_replaced_clone_stops_the_servers() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    trust(&mut host);
    let (held, process) = serve(&mut host, "rust-analyzer", dir.path());
    drop(held);

    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git_in(&repo, &["add", "--all"]);
    git_in(&repo, &["commit", "--quiet", "-m", "again"]);
    host.update(Model::changed);
    let captured = host.run_newest_job();
    assert_eq!(host.model().parts().servers.len(), 0);
    assert!(stopped(process), "the server runs in another clone");
    host.complete(captured);
    assert_eq!(host.model().replaced(), Some(Replaced::Clone));
    assert_eq!(host.model().trust, Trust::Denied);

    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    assert!(
        host.model().status.starts_with(
            "Go to definition needs the code on disk. This checkout was replaced by another clone"
        ),
        "{}",
        host.model().status
    );
    assert!(!host.model().looking_up, "a lookup runs");
}

/// `src/main.rs` of [`rust_checkout_host`]'s crate, calling `function` of `src/util.rs` at the
/// same place whatever its name.
fn calling(function: &str) -> String {
    formatdoc! {"
        mod util;

        fn main() {{
            let _ = util::{function}();
        }}
    "}
}

/// The window over the review of a checkout under `dir` of a Rust crate whose `src/util.rs`
/// defines `helper` on its first line and `other` on its fifth, and whose `src/main.rs` calls
/// `helper`, uncommitted; returns it with the working tree.
fn rust_checkout_host(dir: &Path) -> (Host, std::path::PathBuf) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    // A workspace of its own, which the project's around `target/` does not claim, whose build
    // output git ignores, as in any Rust checkout.
    std::fs::write(
        repo.join("Cargo.toml"),
        indoc! {r#"
            [package]
            name = "probe"
            version = "0.1.0"
            edition = "2024"

            [workspace]
        "#},
    )
    .unwrap();
    std::fs::write(
        repo.join(".gitignore"),
        indoc! {"
            /target
            /Cargo.lock
        "},
    )
    .unwrap();
    std::fs::write(
        repo.join("src/util.rs"),
        indoc! {"
            pub fn helper() -> u32 {
                7
            }

            pub fn other() -> u32 {
                8
            }
        "},
    )
    .unwrap();
    std::fs::write(
        repo.join("src/main.rs"),
        indoc! {"
            mod util;

            fn main() {}
        "},
    )
    .unwrap();
    git_in(&repo, &["add", "--all"]);
    git_in(&repo, &["commit", "--quiet", "-m", "base"]);
    git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    std::fs::write(repo.join("src/main.rs"), calling("helper")).unwrap();
    let loaded = crate::launch::open_checkout(
        &dir.join("data"),
        review_core::Repo::discover(&repo).unwrap(),
    )
    .unwrap();
    (Host::open_loaded(loaded, settings()), repo)
}

/// Opens `src/main.rs` of [`rust_checkout_host`]'s crate in the buffer with the cursor on the
/// function it calls.
fn buffer_on_the_call(host: &mut Host) {
    host.update(|model| model.show_file("src/main.rs"));
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleBuffer);
    host.update(|model| model.set_buffer_cursor(3, "    let _ = util::".len()));
    assert_eq!(host.model().mode, Mode::Buffer);
}

/// Go to definition runs a language server in the checkout and follows a call into another
/// file; after an edit moves the call to another function, the same server answers about the
/// edited text; skipped where rust-analyzer is not installed.
#[test]
fn go_to_definition_runs_in_the_checkout() {
    if !crate::lsp::rust_analyzer_installed() {
        eprintln!("skipped: rust-analyzer is not installed");
        return;
    }
    let dir = crate::lsp::scratch_dir();
    let (mut host, repo) = rust_checkout_host(dir.path());
    trust(&mut host);
    let look_up = |host: &mut Host, row: usize| {
        buffer_on_the_call(host);
        host.dispatch(Action::GoToDefinition);
        host.settle();
        let model = host.model();
        let buffer = model.buffer.as_ref().unwrap();
        assert_eq!(buffer.title(), "src/util.rs", "{}", model.status);
        assert_eq!(buffer.cursor.row, row, "{}", model.status);
    };
    look_up(&mut host, 0);

    // The call names `other` now, and a capture shows it.
    std::fs::write(repo.join("src/main.rs"), calling("other")).unwrap();
    host.update(Model::changed);
    host.settle();
    host.dispatch(Action::ToggleBuffer);
    look_up(&mut host, 4);
    assert_eq!(host.model().parts().servers.len(), 1, "the server was kept");
    host.model().parts().servers.stop();
}

/// A shell script standing in for a language server, speaking just enough of the protocol: it
/// answers the handshake and the request to exit, and answers every definition request with
/// line `$2` of the file `$1`, but only once the directory `$3` holds `go`, after it put
/// `asked` there.
#[cfg(target_os = "linux")]
const CUED_SERVER: &str = indoc! {r#"
    target=$1 line=$2 markers=$3
    reply() {
        printf 'Content-Length: %s\r\n\r\n%s' "$(printf %s "$1" | wc -c | tr -d ' ')" "$1"
    }
    while :; do
        length=
        while IFS= read -r header; do
            header=$(printf %s "$header" | tr -d '\r')
            [ -z "$header" ] && break
            case $header in Content-Length:*) length=${header#Content-Length: } ;; esac
        done
        [ -n "$length" ] || exit 0
        body=$(dd bs=1 count="$length" 2>/dev/null)
        id=$(printf %s "$body" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
        case $body in
            *'"method":"initialize"'*)
                reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{}}}" ;;
            *'"method":"textDocument/definition"'*)
                : > "$markers/asked"
                waited=0
                while [ ! -e "$markers/go" ] && [ "$waited" -lt 6000 ]; do
                    sleep 0.01
                    waited=$((waited + 1))
                done
                at="{\"line\":$line,\"character\":0}"
                reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"uri\":\"file://$target\",\"range\":{\"start\":$at,\"end\":$at}}}" ;;
            *'"method":"shutdown"'*)
                reply "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
            *'"method":"exit"'*) exit 0 ;;
        esac
    done
"#};

/// Puts [`CUED_SERVER`] in the window's servers as the language server for Rust in `root`,
/// answering with line `line` of the file `target` once `markers` holds `go`.
#[cfg(target_os = "linux")]
fn cued_server(host: &mut Host, root: &Path, target: &Path, line: usize, markers: &Path) {
    let line = line.to_string();
    let args = [
        "-c",
        CUED_SERVER,
        "cued-server",
        target.to_str().unwrap(),
        &line,
        markers.to_str().unwrap(),
    ];
    let server = crate::lsp::LanguageServer::stand_in("sh", &args, root).unwrap();
    drop(host.model().parts().servers.insert("rust-analyzer", server));
}

/// Waits until `path` exists, ten seconds at most.
#[cfg(target_os = "linux")]
fn wait_for(path: &Path) {
    let waited = Instant::now();
    while !path.exists() {
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "{} never appears",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// An answer that arrives once the code on disk moved past what shows is dropped, though that
/// code showed when the lookup began: the server answers only once a new file landed.
#[cfg(target_os = "linux")]
#[test]
fn an_answer_is_dropped_once_the_disk_moves() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    let markers = dir.path().join("markers");
    std::fs::create_dir(&markers).unwrap();
    trust(&mut host);
    buffer_on_a_name(&mut host);
    let target = repo.canonicalize().unwrap().join("a.rs");
    cued_server(&mut host, &repo, &target, 2, &markers);

    // With the disk as it shows, the answer lands.
    let asked_at = host.model().buffer.as_ref().unwrap().cursor;
    std::fs::write(markers.join("go"), "").unwrap();
    host.dispatch(Action::GoToDefinition);
    host.settle();
    assert_eq!(host.model().status, "a.rs:3");
    std::fs::remove_file(markers.join("go")).unwrap();
    std::fs::remove_file(markers.join("asked")).unwrap();
    host.update(|model| model.set_buffer_cursor(asked_at.row, asked_at.col));

    // A file lands while the server works on the question.
    host.dispatch(Action::GoToDefinition);
    let lookup = host.jobs.pop_back().expect("the lookup waits");
    let running = std::thread::spawn(move || lookup.run());
    wait_for(&markers.join("asked"));
    std::fs::write(repo.join("b.rs"), "fn b() {}\n").unwrap();
    std::fs::write(markers.join("go"), "").unwrap();
    let answered = running.join().unwrap();
    assert!(
        matches!(
            &*answered.outcome,
            Outcome::Definition(Err(super::buffer::Unanswered::Moved))
        ),
        "the answer stands"
    );
    host.complete(answered);
    assert!(
        host.model().status.contains("no longer matches what shows"),
        "{}",
        host.model().status
    );
}

/// A definition in a file whose text on disk is not the snapshot's, as a file git keeps at its
/// staged version under a filter driver, is refused, though the tree on disk is the
/// snapshot's.
#[cfg(target_os = "linux")]
#[test]
fn a_definition_in_a_file_unlike_the_snapshot_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (review, repo) = worktree_review_of(
        dir.path(),
        &[
            ("a.rs", &two_hunks("1", "2")),
            (".gitattributes", "util.rs filter=crypt\n"),
            ("util.rs", "fn helper() {}\n"),
        ],
        &[
            ("a.rs", &two_hunks("one", "two")),
            (
                "util.rs",
                indoc! {"
                    // edited, and kept at its staged version
                    fn helper() {}
                "},
            ),
        ],
    );
    let mut host = Host::open(review);
    let markers = dir.path().join("markers");
    std::fs::create_dir(&markers).unwrap();
    std::fs::write(markers.join("go"), "").unwrap();
    trust(&mut host);
    buffer_on_a_name(&mut host);
    let target = repo.canonicalize().unwrap().join("util.rs");
    cued_server(&mut host, &repo, &target, 0, &markers);

    host.dispatch(Action::GoToDefinition);
    host.settle();
    let model = host.model();
    assert_eq!(
        model.status,
        "Go to definition: util.rs on disk is not what shows"
    );
    assert_eq!(model.buffer.as_ref().unwrap().title(), "a.rs");
}

/// An untrusted lookup asks for trust only once the disk is found to hold what shows: on a
/// view the disk moved past, it says why it cannot run and asks nothing.
#[test]
fn an_untrusted_lookup_asks_for_trust_only_for_code_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    buffer_on_a_name(&mut host);
    std::fs::write(repo.join("b.rs"), "fn b() {}\n").unwrap();
    host.dispatch(Action::GoToDefinition);
    assert_eq!(host.model().overlay, None);
    host.settle();
    let model = host.model();
    assert_eq!(model.overlay, None);
    assert_eq!(model.trust, Trust::Denied);
    assert!(
        model.status.contains("no longer matches what shows"),
        "{}",
        model.status
    );

    std::fs::remove_file(repo.join("b.rs")).unwrap();
    host.dispatch(Action::GoToDefinition);
    host.settle();
    assert_eq!(host.model().overlay, Some(Overlay::Trust));
}

/// Fires the watch on the grant, as its time comes, and lands the reading it starts.
#[cfg(target_os = "linux")]
fn fire_guard(host: &mut Host) {
    let position = host
        .clocks
        .iter()
        .position(|clock| matches!(*clock.outcome, Outcome::Tick(super::sync::Tick::Guard)))
        .expect("the watch on the grant is due");
    let tick = host.clocks.remove(position);
    host.complete(tick);
    host.settle();
}

/// Starts a lookup, which arms the watch on the grant, without running it, as a language
/// server would start; returns the stand-in for that server, held by nothing, and its process.
#[cfg(target_os = "linux")]
fn looked_up(host: &mut Host, root: &Path) -> u32 {
    buffer_on_a_name(host);
    host.dispatch(Action::GoToDefinition);
    drop(host.jobs.pop_back());
    host.update(|model| model.looking_up = false);
    let (held, process) = serve(host, "rust-analyzer", root);
    drop(held);
    process
}

/// A revocation in another window stops this window's language servers within the watch's
/// period, though no capture shows to read the grant again: the window follows nothing and an
/// overlay is open.
#[cfg(target_os = "linux")]
#[test]
fn a_revocation_elsewhere_stops_the_servers_whatever_shows() {
    let dir = tempfile::tempdir().unwrap();
    let (review, _repo) = worktree_review_of(
        dir.path(),
        &[("a.rs", &two_hunks("1", "2"))],
        &[("a.rs", &two_hunks("one", "two"))],
    );
    let mut host = Host::open_with(
        review,
        Settings {
            live: false,
            ..settings()
        },
    );
    trust(&mut host);
    let process = looked_up(&mut host, dir.path());
    host.dispatch(Action::ToggleBuffer);
    host.dispatch(Action::ToggleHelp);
    assert_eq!(host.model().overlay, Some(Overlay::Help));

    checkout_review(&host).set_trusted(false).unwrap();
    fire_guard(&mut host);
    assert_eq!(host.model().parts().servers.len(), 0);
    assert!(stopped(process), "the server still runs");
    assert_eq!(host.model().trust, Trust::Denied);
    assert!(
        host.model()
            .status
            .starts_with("Trust in this checkout no longer applies"),
        "{}",
        host.model().status
    );
    assert!(
        !host
            .clocks
            .iter()
            .any(|clock| matches!(*clock.outcome, Outcome::Tick(super::sync::Tick::Guard))),
        "the watch goes on without a server"
    );
}

/// While git is busy in the checkout, no capture shows, and the watch on the grant still stops
/// the language servers once another window revoked it.
#[cfg(target_os = "linux")]
#[test]
fn a_revocation_elsewhere_stops_the_servers_while_git_is_busy() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    trust(&mut host);
    let process = looked_up(&mut host, dir.path());

    std::fs::write(repo.join(".git/index.lock"), "").unwrap();
    host.review.set_trusted(false).unwrap();
    host.update(Model::changed);
    host.settle();
    assert!(host.model().live_state().unwrap().busy.is_some());
    assert!(!stopped(process), "the busy capture read the grant");

    fire_guard(&mut host);
    assert!(stopped(process), "the server still runs");
    assert_eq!(host.model().trust, Trust::Denied);
}

/// The checkout of a review of the working tree replaced by another clone stops its language
/// servers at the watch's next reading, through the checkout's review bound as the window
/// opened, though the review's own key names no clone.
#[cfg(target_os = "linux")]
#[test]
fn a_replaced_clone_stops_the_servers_of_a_working_tree_review() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    trust(&mut host);
    let process = looked_up(&mut host, dir.path());

    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git_in(&repo, &["add", "--all"]);
    git_in(&repo, &["commit", "--quiet", "-m", "again"]);
    fire_guard(&mut host);
    assert_eq!(host.model().parts().servers.len(), 0);
    assert!(stopped(process), "the server runs in another clone");
    assert_eq!(host.model().trust, Trust::Denied);
}

/// A review of a range that learns of a newer revision reads no trust, as it has no checkout.
#[test]
fn a_range_with_a_newer_revision_reads_no_trust() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    prepare(
        &DemoPaths::under(dir.path()),
        &fixture_dir(),
        DemoRequest::UpTo(review_core::RevisionNumber::new(2).unwrap()),
    )
    .unwrap();
    host.dispatch(Action::Refresh);
    let refreshed = host.run_newest_job();
    host.complete(refreshed);
    assert_eq!(
        host.model()
            .snapshot
            .as_ref()
            .unwrap()
            .newer()
            .map(review_core::RevisionNumber::get),
        Some(2)
    );
    assert_eq!(host.model().trust, Trust::Denied, "trust is read");
    assert!(host.jobs.is_empty(), "a job runs");
}

/// A capture that waits behind an overlay never shows once the watch saw a change after it:
/// the view stays, as updating, until the capture of that change lands.
#[test]
fn a_waiting_capture_older_than_a_change_never_shows() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    let shown = host.model().shown;
    host.dispatch(Action::ToggleHelp);
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    let capture = host.run_newest_job();
    host.complete(capture);
    assert!(host.model().live.as_ref().unwrap().has_waiting());

    std::fs::write(repo.join("a.rs"), two_hunks("one", "two")).unwrap();
    host.update(Model::changed);
    host.dispatch(Action::CloseOverlay);
    host.draw();
    let model = host.model();
    assert_eq!(
        model.shown, shown,
        "the waiting capture is older than the change"
    );
    assert!(model.live.as_ref().unwrap().updating());

    host.settle();
    let model = host.model();
    assert_eq!(model.shown, shown + 1);
    assert!(!model.live.as_ref().unwrap().updating());
    assert_eq!(
        model.totals(),
        (0, 2),
        "the newest capture shows, with both hunks"
    );
}

/// The position in the visible tree of the folder at `path`.
fn folder_at(model: &Model, path: &str) -> usize {
    model
        .visible_tree()
        .iter()
        .position(|&index| {
            let node = &model.tree[index];
            node.kind == NodeKind::Directory && node.path == path
        })
        .expect("the demo has the folder")
}

/// The files of the folder at `path`, in tree order.
fn files_under(model: &Model, path: &str) -> Vec<String> {
    model
        .tree
        .iter()
        .filter(|node| node.kind == NodeKind::File && review_core::tree::is_under(&node.path, path))
        .map(|node| node.path.clone())
        .collect()
}

/// On a folder, the diff shows every change of the folder's files, each under its path and in
/// the hunks view, as lazygit shows a folder's diff; on a file again, that file's diff shows
/// where the reviewer left it.
#[test]
fn a_folder_under_the_cursor_shows_its_files_diffs() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    let engine = "src/engine.rs";
    host.update(|model| model.show_file(engine));
    lay_out(&mut host);
    let top = host.model().diff_scroll.offset();
    let src = folder_at(host.model(), "src");
    host.update(|model| model.tree_move_to(src));

    // The folder's diff: a section per file in tree order, under a header row each, with no
    // file shown in place of it.
    let expected = files_under(host.model(), "src");
    assert!(expected.len() > 1, "src has several files");
    let model = host.model();
    assert_eq!(model.folder(), Some("src"));
    assert_eq!(model.shown_file(), None);
    let sections: Vec<&str> = model
        .rows
        .files
        .iter()
        .map(|section| section.path.as_str())
        .collect();
    assert_eq!(sections, expected);
    let headers = model
        .rows
        .rows
        .iter()
        .filter(|row| row.kind() == rows::RowKind::FileHeader)
        .count();
    assert_eq!(headers, expected.len());
    assert_eq!(
        model.rows.presentation.view,
        crate::config::DiffView::Hunks,
        "a folder's diff shows its hunks apart, whatever the view"
    );

    // Another folder shows its own files.
    let docs = folder_at(host.model(), "docs");
    host.update(|model| model.tree_move_to(docs));
    assert_eq!(host.model().folder(), Some("docs"));
    assert_eq!(
        host.model().rows.files[0].path,
        files_under(host.model(), "docs")[0]
    );

    // The engine again, where it was left.
    host.update(|model| model.show_file(engine));
    let model = host.model();
    assert_eq!(model.folder(), None);
    assert_eq!(model.shown_file(), Some(engine));
    assert_eq!(model.rows.files.len(), 1);
    assert!((model.diff_scroll.offset() - top).abs() < f64::EPSILON);
}

/// Tab on a folder opens the file its diff shows at the reading line, with the same line at
/// the same place on screen, and the diff and the tree then act on that file.
#[test]
fn opening_a_folders_diff_opens_the_file_at_its_reading_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    // A diff ten rows tall, which the demo's short files more than fill.
    let short = Geometry {
        heights: Heights {
            diff: Some(10.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    let lay_out = |host: &mut Host| host.update(|model| model.layout(short, Instant::now()));
    let src = folder_at(host.model(), "src");
    host.update(|model| model.tree_move_to(src));
    lay_out(&mut host);

    // The reading line on a changed line of a file after the first with a screenful of the
    // file before and after it, so the file's own diff can put it at the same place.
    let (second, row) = {
        let model = host.model();
        let row = (0..model.rows.rows.len())
            .find(|&row| {
                let row = &model.rows.rows[row];
                let lines = model.rows.files[row.file].lines.unwrap_or(0);
                row.file > 0
                    && row.hunk.is_some()
                    && row
                        .cells()
                        .next()
                        .and_then(|cell| cell.head.as_ref())
                        .is_some_and(|head| head.line >= 5 && head.line + 10 < lines)
            })
            .expect("a later file of src changes a line with a screenful around it");
        let file = model.rows.rows[row].file;
        (model.rows.files[file].path.clone(), row)
    };
    let unit = host.model().rows.rows[row].unit;
    let reading = (10.0 * scroll::READING_LINE).floor();
    host.update(|model| model.drag_list_to(List::Diff, scroll::rows(row) - reading));
    lay_out(&mut host);
    let on_screen = scroll::rows(row) - host.model().diff_scroll.offset();
    let line = host.model().rows.rows[row]
        .cells()
        .next()
        .map(|cell| (cell.head.as_ref().map(|head| head.line), cell.base));

    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    let model = host.model();
    assert_eq!(model.mode, Mode::Diff);
    assert_eq!(model.folder(), None);
    assert_eq!(model.shown_file(), Some(second.as_str()));
    assert_eq!(model.tree_node().unwrap().path, second, "the tree follows");
    let at = scroll::whole_rows(model.diff_scroll.offset() + on_screen);
    let landed = model.rows.rows[at]
        .cells()
        .next()
        .map(|cell| (cell.head.as_ref().map(|head| head.line), cell.base));
    assert_eq!(landed, line, "the same line, where it was on screen");
    assert!(
        model.hunk_units().contains(&unit),
        "the hunk of the change at the reading line is selected"
    );
}

/// A folder's diff shows the files the tree's filter keeps, and widens to all of them once the
/// filter clears, so a review key on the folder acts on exactly the files its diff shows.
#[test]
fn a_folders_diff_follows_the_filter() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| {
        model.filter = Some(super::Filter {
            id: 0,
            query: "src/engine".to_owned(),
            editing: false,
        });
        model.apply_filter();
    });
    let src = folder_at(host.model(), "src");
    host.update(|model| model.tree_move_to(src));
    let sections = |model: &Model| -> Vec<String> {
        model
            .rows
            .files
            .iter()
            .map(|section| section.path.clone())
            .collect()
    };
    assert_eq!(sections(host.model()), ["src/engine.rs"]);

    // Cleared, the filter leaves the cursor on the folder, whose diff shows all its files.
    host.update(|model| {
        model.filter = None;
        model.apply_filter();
    });
    assert_eq!(host.model().tree_node().unwrap().path, "src");
    let all = files_under(host.model(), "src");
    assert_eq!(sections(host.model()), all);

    // A review key acts on exactly the files the folder's diff shows.
    let (mut target, _) = host.model().review_target().unwrap();
    let view = host.model().view().unwrap();
    let mut shown: Vec<_> = all
        .iter()
        .flat_map(|path| view.units_of_file(path))
        .collect();
    shown.sort_unstable();
    target.sort_unstable();
    assert_eq!(target, shown);
}

/// A folder's diff is the tree's alone: leaving the tree any other way opens a file, so the
/// diff's keys never act on a folder's rows.
#[test]
fn leaving_the_tree_from_a_folder_opens_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let src = folder_at(host.model(), "src");
    host.update(|model| model.tree_move_to(src));
    lay_out(&mut host);
    assert_eq!(host.model().folder(), Some("src"));
    host.dispatch(Action::ToggleSidebar);
    let model = host.model();
    assert_eq!(model.mode, Mode::Diff);
    assert_eq!(model.folder(), None);
    let shown = model.shown_file().unwrap();
    assert!(review_core::tree::is_under(shown, "src"), "{shown}");
    assert_eq!(model.rows.files[0].path, shown);
}

/// A notch of the wheel scrolls its rows; notches kept coming fast scroll further, and a
/// precise scroll, such as a touchpad's, always scrolls exactly its rows.
#[test]
fn the_wheel_builds_momentum_in_the_diff() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("scheduler.rs"));
    lay_out(&mut host);
    host.update(|model| model.drag_list_to(List::Diff, 0.0));
    let start = Instant::now();
    let notch = |host: &mut Host, at: Instant| {
        let before = host.model().diff_scroll.offset();
        host.update(|model| model.wheel_list(List::Diff, Wheel::Notches { rows: 3.0, at }));
        host.model().diff_scroll.offset() - before
    };
    assert!((notch(&mut host, start) - 3.0).abs() < f64::EPSILON);
    let mut last = 0.0;
    for step in 1..20 {
        last = notch(&mut host, start + Duration::from_millis(20 * step));
    }
    assert!(last > 6.0, "a fast wheel scrolls further: {last}");
    let before = host.model().diff_scroll.offset();
    host.update(|model| model.wheel_list(List::Diff, Wheel::Precise(3.0)));
    assert!((host.model().diff_scroll.offset() - before - 3.0).abs() < f64::EPSILON);
}

/// `f` switches between the whole file and hunks apart wherever `v` does.
#[test]
fn f_switches_the_view() {
    let (bindings, _) = keymap::keymap(&BTreeMap::new());
    for area in [Area::Tree, Area::Diff] {
        assert!(
            bindings.iter().any(|binding| binding.action == "ToggleView"
                && binding.area == area
                && binding.keys.iter().any(|key| key == "f")),
            "{area:?}"
        );
    }
}

/// A click on a change in a folder's diff opens its file with the change clicked selected.
#[test]
fn a_click_in_a_folders_diff_selects_the_change_clicked() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    let folders: Vec<usize> = (0..host.model().visible_tree().len())
        .filter(|&position| {
            let model = host.model();
            model.tree[model.visible_tree()[position]].kind == NodeKind::Directory
        })
        .collect();
    // A folder whose diff has a changed line of a file after its first.
    let (row, unit) = folders
        .into_iter()
        .find_map(|position| {
            host.update(|model| model.tree_move_to(position));
            let model = host.model();
            model.rows.rows.iter().enumerate().find_map(|(index, row)| {
                row.hunk?;
                let later = row.file > 0;
                let changed = row.cells().next().is_some_and(|cell| {
                    matches!(cell.kind, rows::RowKind::Added | rows::RowKind::Removed)
                });
                (later && changed).then_some((index, row.unit))
            })
        })
        .expect("a folder's diff has a changed line of a later file");
    lay_out(&mut host);
    host.update(|model| model.press_text(text_press(row, 0)));
    let model = host.model();
    assert_eq!(model.mode, Mode::Diff);
    assert_eq!(model.folder(), None);
    assert!(
        model.hunk_units().contains(&unit),
        "the change clicked is selected"
    );
}

/// The defaults with each hunk apart.
fn hunks_apart() -> Settings {
    let mut settings = settings();
    settings.appearance.applied.view = crate::config::DiffView::Hunks;
    settings
}

/// In the hunks view, fewer than ten unchanged lines between two changes show whole, while a
/// longer stretch folds into a gap row.
#[test]
fn a_few_unchanged_lines_between_changes_show_whole() {
    let dir = tempfile::tempdir().unwrap();
    let text = |changed: &[usize]| -> String {
        let lines: Vec<String> = (0..40)
            .map(|line| {
                if changed.contains(&line) {
                    format!("changed {line}")
                } else {
                    format!("line {line}")
                }
            })
            .collect();
        lines.join("\n") + "\n"
    };
    let base = text(&[]);
    // Changes at lines 2, 11 and 30: eight unchanged lines between the first two, eighteen
    // between the last two.
    let head = text(&[2, 11, 30]);
    let (review, _repo) = worktree_review_of(dir.path(), &[("a.txt", &base)], &[("a.txt", &head)]);
    let mut host = Host::open_with(review, hunks_apart());
    host.update(|model| model.show_file("a.txt"));
    let model = host.model();
    let lines: Vec<usize> = model
        .rows
        .rows
        .iter()
        .filter(|row| row.kind() == rows::RowKind::Context)
        .filter_map(|row| row.cells().next()?.head.as_ref().map(|head| head.line))
        .collect();
    assert!(
        (3..11).all(|line| lines.contains(&line)),
        "the eight lines between the first changes show: {lines:?}"
    );
    let gaps: Vec<&str> = model
        .rows
        .rows
        .iter()
        .filter_map(|row| match &row.body {
            rows::Body::Note(note) if note.kind == rows::RowKind::Gap => Some(note.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(gaps, ["12 unchanged lines"], "only the long stretch folds");
}

/// Switching the view keeps the code exactly where it is on screen, also when a row only the
/// hunks view has, a gap of unchanged lines, is at the top.
#[test]
fn switching_the_view_keeps_the_code_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), hunks_apart());
    // A diff ten rows tall, which the demo's files more than fill.
    let short = Geometry {
        heights: Heights {
            diff: Some(10.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    let lay_out = |host: &mut Host| host.update(|model| model.layout(short, Instant::now()));
    host.update(|model| model.show_file("scheduler.rs"));
    lay_out(&mut host);
    // A gap at the top, past the file's start.
    let gap = host
        .model()
        .rows
        .rows
        .iter()
        .enumerate()
        .skip(5)
        .find(|(_, row)| row.kind() == rows::RowKind::Gap)
        .map(|(index, _)| index)
        .expect("scheduler.rs has gaps");
    host.update(|model| model.drag_list_to(List::Diff, scroll::rows(gap)));
    lay_out(&mut host);
    let screen = |model: &Model| -> Vec<Option<usize>> {
        let top = scroll::whole_rows(model.diff_scroll.offset());
        (top..top + 10)
            .map(|row| {
                model
                    .rows
                    .rows
                    .get(row)?
                    .cells()
                    .next()?
                    .head
                    .as_ref()
                    .map(|head| head.line)
            })
            .collect()
    };
    let before = screen(host.model());
    assert_eq!(before[0], None, "the gap is at the top");
    host.dispatch(Action::ToggleView);
    lay_out(&mut host);
    let after = screen(host.model());
    // Every line the hunks view showed below the gap, up to the next row it alone has, is at
    // the same row in the file view.
    for (row, line) in before.iter().enumerate().skip(1) {
        if line.is_none() {
            break;
        }
        assert_eq!(after[row], *line, "row {row}: {before:?} then {after:?}");
    }
}

/// Moving from a folder onto its first file keeps the file's code on the same rows: the empty
/// row a single file's diff starts with stands where the folder's diff names the file.
#[test]
fn a_folder_and_its_first_file_show_the_code_on_the_same_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), hunks_apart());
    let folder = folder_at(host.model(), "src/generated");
    host.update(|model| model.tree_move_to(folder));
    let lines = |model: &Model| -> Vec<Option<usize>> {
        model
            .rows
            .rows
            .iter()
            .take(8)
            .map(|row| row.cells().next()?.head.as_ref().map(|head| head.line))
            .collect()
    };
    let in_folder = lines(host.model());
    assert_eq!(host.model().rows.files.len(), 1, "the folder has one file");
    host.dispatch(Action::TreeDown);
    assert_eq!(host.model().shown_file(), Some("src/generated/schema.rs"));
    lay_out(&mut host);
    assert!((host.model().diff_scroll.offset()).abs() < f64::EPSILON);
    assert_eq!(lines(host.model()), in_folder);
}

/// The window over a working-tree review of `path`, a 60-line function accepted at one
/// revision whose line 16 changed at the next, showing `path`; the directory holds the
/// repository.
fn accepted_then_changed(dir: &Path, path: &str) -> Host {
    accepted_then_edited(dir, path, &[15])
}

/// The window over a working-tree review of `path`, a 60-line function accepted at one
/// revision whose statements `edited` changed at the next, showing `path`; statement `n` is on
/// line `n + 2`, and the directory holds the repository.
fn accepted_then_edited(dir: &Path, path: &str, edited: &[usize]) -> Host {
    accepted_then(
        dir,
        path,
        &statements(edited, "1"),
        &statements(edited, "2"),
    )
}

/// A 60-line function whose statements `edited` assign `label`, the others `0`.
fn statements(edited: &[usize], label: &str) -> String {
    use std::fmt::Write as _;

    let mut text = "fn a() {\n".to_owned();
    for line in 0..60 {
        let value = if edited.contains(&line) { label } else { "0" };
        writeln!(text, "    let v{line} = {value};").unwrap();
    }
    text.push_str("}\n");
    text
}

/// The window over a working-tree review of `path`, `first` accepted at one revision and
/// `second` at the next, showing `path`, beside a new change to `b.rs`; the directory holds
/// the repository.
fn accepted_then(dir: &Path, path: &str, first: &str, second: &str) -> Host {
    accepted_below(dir, path, "", first, second, settings())
}

/// [`accepted_then`] with `above` above the function in every version, base too.
fn accepted_below(
    dir: &Path,
    path: &str,
    above: &str,
    first: &str,
    second: &str,
    settings: Settings,
) -> Host {
    let base = formatdoc! {"
        {above}fn a() {{
        }}
    "};
    let (first, second) = (format!("{above}{first}"), format!("{above}{second}"));
    let (first, second) = (first.as_str(), second.as_str());
    let (review, repo) = worktree_review_of(
        dir,
        &[(path, &base), ("b.rs", "fn b() {}\n")],
        &[(path, first), ("b.rs", "fn b() { b(); }\n")],
    );
    let history = review.history().unwrap();
    let view = history.current_view().unwrap();
    review
        .mark(&view, &view.units_of_file(path), MarkVerdict::Accepted)
        .unwrap();
    std::fs::write(repo.join(path), second).unwrap();
    review.refresh().unwrap();
    let mut host = Host::open_with(review, settings);
    host.update(|model| model.show_file(path));
    host.settle();
    host
}

/// The head lines each change since review starts at, of the hunks that are one such change.
fn run_lines(model: &Model) -> Vec<usize> {
    model.rows.hunks.iter().filter_map(rows::Hunk::at).collect()
}

/// A long change accepted at one revision whose one line changed at the next shows what changed
/// on top of it, as a new change shows: in the hunks view the hunk is the changed line alone,
/// with the accepted lines around it as context, colored by the head they were accepted in, and
/// the rest folded into gaps.
#[test]
fn a_change_since_review_shows_on_top_of_the_accepted_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_changed(dir.path(), "a.rs");

    let rows = &host.model().rows.rows;
    let changed: Vec<&rows::Row> = rows
        .iter()
        .filter(|row| {
            matches!(
                row.kind(),
                rows::RowKind::Edited | rows::RowKind::Removed | rows::RowKind::Added
            )
        })
        .collect();
    let [edited] = changed.as_slice() else {
        panic!("one line changed since the review: {changed:?}");
    };
    assert_eq!(edited.kind(), rows::RowKind::Edited);
    assert_eq!(edited.text(), "    let v15 = 12;");
    // The hunk is the change alone, without a header, so its bar runs along nothing else; the
    // lines around it are the same as reviewed, so they come from the head reviewed then and
    // have no base line.
    let hunk: Vec<rows::RowKind> = rows
        .iter()
        .filter(|row| row.hunk.is_some())
        .map(rows::Row::kind)
        .collect();
    assert_eq!(hunk, [rows::RowKind::Edited]);
    let edited_row = rows
        .iter()
        .position(|row| row.kind() == rows::RowKind::Edited)
        .unwrap();
    let rows::Body::Code(before) = &rows[edited_row - 1].body else {
        panic!("context right above the change");
    };
    assert_eq!(before.text, "    let v14 = 0;");
    assert_eq!(before.base, None);
    assert_eq!(before.reviewed.as_ref().map(|line| line.line), Some(15));
    let gaps: Vec<&str> = rows
        .iter()
        .filter(|row| row.kind() == rows::RowKind::Gap)
        .map(rows::Row::text)
        .collect();
    assert_eq!(gaps, ["13 unchanged lines", "42 unchanged lines"]);
    let reviewed = rows
        .iter()
        .find_map(|row| match &row.body {
            rows::Body::Code(cell) => cell.reviewed.clone(),
            rows::Body::Pair(_) | rows::Body::Note(_) => None,
        })
        .expect("the accepted lines know the head they were accepted in");
    let syntax = host.model().folds.syntax.get("a.rs").unwrap();
    assert!(
        syntax.reviewed.contains_key(&reviewed.side),
        "the head it was accepted in is highlighted"
    );

    // Opening the gap after the change shows the lines right after it first.
    let gap = host
        .model()
        .rows
        .rows
        .iter()
        .rposition(|row| row.kind() == rows::RowKind::Gap)
        .unwrap();
    host.update(|model| model.expand_gap(gap));
    let after: Vec<&str> = host
        .model()
        .rows
        .rows
        .iter()
        .skip_while(|row| row.kind() != rows::RowKind::Edited)
        .skip(1)
        .take(4)
        .map(rows::Row::text)
        .collect();
    assert_eq!(
        after,
        [
            "    let v16 = 0;",
            "    let v17 = 0;",
            "    let v18 = 0;",
            "    let v19 = 0;"
        ]
    );
}

/// In the file view, a change since review shows every line, and its hunk, bar and stop are the
/// changed line alone.
#[test]
fn a_change_since_review_is_selected_at_the_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_changed(dir.path(), "a.rs");

    host.dispatch(Action::ToggleView);
    let model = host.model();
    let hunk: Vec<rows::RowKind> = model
        .rows
        .rows
        .iter()
        .filter(|row| row.hunk.is_some())
        .map(rows::Row::kind)
        .collect();
    assert_eq!(hunk, [rows::RowKind::Edited]);
    let start = model.hunk_rows[model.hunk].start;
    assert_eq!(
        model.rows.row_on_screen(start).map(rows::Row::kind),
        Some(rows::RowKind::Edited)
    );
    assert!(
        !model
            .rows
            .rows
            .iter()
            .any(|row| row.kind() == rows::RowKind::Gap),
        "the file view shows every line"
    );
}

/// A noise file that changed since it was reviewed stays folded as noise, and expanded shows its
/// whole change once, every line in order.
#[test]
fn an_expanded_noise_file_changed_since_review_shows_each_line_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_changed(dir.path(), "Cargo.lock");
    let unit = host.model().rows.hunks[0].unit;
    host.update(|model| {
        model.folds.expanded.insert(unit);
        model.rebuild();
    });
    let lines: Vec<usize> = host
        .model()
        .rows
        .rows
        .iter()
        .flat_map(rows::Row::cells)
        .filter_map(|cell| cell.head.as_ref().map(|head| head.line))
        .collect();
    assert_ne!(lines, Vec::<usize>::new());
    assert!(
        lines.windows(2).all(|pair| pair[0] < pair[1]),
        "every head line once, in order: {lines:?}"
    );
}

/// Two distant edits on top of an accepted change are two hunks, each a stop: Space approves
/// only the selected one, which nothing shows approved until it is saved; then its lines are
/// context, the other change is selected, and approving it too accepts the unit.
#[test]
fn each_change_since_review_is_approved_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    let unit = host.model().rows.hunks[0].unit;
    assert_eq!(run_lines(host.model()), [11, 41]);
    assert_eq!(host.model().rows.hunks.len(), 2, "a stop for each change");
    host.dispatch(Action::NextHunk);
    assert_eq!(host.model().hunk, 1);
    host.dispatch(Action::PreviousHunk);
    assert_eq!(host.model().hunk, 0);

    // The approval shows nothing until it is saved, so the other change never looks approved.
    host.dispatch(Action::ToggleApprove);
    assert!(host.model().pending.is_empty());
    assert_eq!(hunk_badges(host.model(), 1), [rows::Badge::Changed]);
    host.settle();
    assert_eq!(run_lines(host.model()), [41]);
    assert_eq!(host.model().hunk, 0, "the change left is selected");
    assert_eq!(hunk_badges(host.model(), 0), [rows::Badge::Changed]);
    assert!(
        host.model().status.contains("1 change of this unit left"),
        "{}",
        host.model().status
    );
    // The approved line is context now, outside every hunk, and in no file of its own; the
    // file view shows it.
    host.dispatch(Action::ToggleView);
    let approved = host
        .model()
        .rows
        .rows
        .iter()
        .find(|row| {
            row.cells()
                .any(|cell| cell.head.as_ref().is_some_and(|head| head.line == 11))
        })
        .unwrap();
    assert_eq!(approved.kind(), rows::RowKind::Context);
    assert_eq!(approved.hunk, None);
    let rows::Body::Code(cell) = &approved.body else {
        panic!("a code row");
    };
    assert_eq!((cell.base, cell.reviewed.as_ref()), (None, None));
    host.dispatch(Action::ToggleView);

    host.dispatch(Action::ToggleApprove);
    host.settle();
    let view = host.model().view().unwrap();
    assert!(
        matches!(
            view.state(unit).unwrap(),
            review_core::UnitState::Reviewed {
                verdict: review_core::Verdict::Accepted,
                ..
            }
        ),
        "the last change accepts the unit"
    );
}

/// Switching between the hunks and the file view keeps the selected change, though every
/// change of the unit has the same unit ID.
#[test]
fn switching_views_keeps_the_selected_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    for _ in 0..2 {
        host.dispatch(Action::ToggleView);
        let model = host.model();
        assert_eq!(model.rows.hunks[model.hunk].at(), Some(41));
    }
}

/// In the buffer, the review keys act on the change under the cursor, and on nothing on a line
/// the same as reviewed between two changes, never on the whole unit.
#[test]
fn the_buffer_approves_only_the_change_under_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleBuffer);
    assert_eq!(host.model().mode, Mode::Buffer);
    assert_eq!(host.model().buffer.as_ref().unwrap().cursor.row, 41);
    assert_eq!(host.model().buffer_hunk(), Some(1));

    host.update(|model| model.buffer.as_mut().unwrap().cursor.row = 25);
    assert_eq!(host.model().buffer_hunk(), None);
    host.dispatch(Action::ToggleApprove);
    assert!(host.model().saves.is_empty() && !host.model().saving);

    host.update(|model| model.buffer.as_mut().unwrap().cursor.row = 41);
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert_eq!(run_lines(host.model()), [11]);
}

/// Flagging a change since review flags the whole unit, which then shows whole as one hunk.
#[test]
fn a_flag_on_a_change_since_review_flags_the_unit() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleFlag);
    host.settle();
    let model = host.model();
    assert_eq!(model.rows.hunks.len(), 1);
    assert_eq!(model.rows.hunks[0].change, None);
    assert_eq!(hunk_badges(model, 0), [rows::Badge::Flagged]);
}

/// Change `index` of `unit` as a view of the review read now shows it.
fn shown_change(review: &Review, unit: UnitId, index: usize) -> review_core::ShownRun {
    let history = review.history().unwrap();
    let view = history.view(unit.revision()).unwrap();
    let Ok(Some(review_core::SinceReviewed::Delta(delta))) = view.since_reviewed(unit) else {
        panic!("{unit} shows what changed since it was reviewed");
    };
    delta.runs()[index].shown()
}

/// An approval of a change another window approved meanwhile is refused and says so, as a
/// failure to save: a quit waiting for it stops, so the reviewer sees it; the rows show what is
/// left.
#[test]
fn an_approval_of_a_change_approved_meanwhile_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    let unit = host.model().rows.hunks[0].unit;
    // Another window approves the first change.
    let review = host.review.clone();
    let history = review.history().unwrap();
    review
        .approve_run(
            &history.view(unit.revision()).unwrap(),
            &shown_change(&review, unit, 0),
        )
        .unwrap();

    // The keys come as a front end delivers them; Space moves on to the second change at once.
    host.key(Action::ToggleApprove, false);
    assert_eq!(host.model().rows.hunks[host.model().hunk].at(), Some(41));
    host.key(Action::Quit, false);
    host.settle();
    assert_eq!(host.quits, 0, "the refusal keeps the app open");
    assert_eq!(host.model().quitting, Quitting::Stopped);
    assert!(
        host.model()
            .status
            .starts_with("Nothing approved at a.rs:12:"),
        "{}",
        host.model().status
    );
    assert_eq!(
        run_lines(host.model()),
        [41],
        "the second change is still open"
    );
    let model = host.model();
    assert_eq!(
        model.rows.hunks[model.hunk].at(),
        Some(41),
        "the selection stays"
    );
}

/// Space on one change and then on the other before the first save lands approves both: an
/// approval saving never turns a press away, and the second still finds its change. The change
/// on its way shows approved alone, never its unit's other changes, and a press on it again
/// waits for it.
#[test]
fn changes_pressed_while_another_saves_are_all_approved() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    let unit = host.model().rows.hunks[0].unit;
    host.dispatch(Action::ToggleApprove);
    let model = host.model();
    assert_eq!(model.hunk, 1, "on to the second change at once");
    assert_eq!(model.hunk_badge(0), Some(rows::Badge::Accepted));
    assert_eq!(model.hunk_badge(1), Some(rows::Badge::Changed));
    host.dispatch(Action::PreviousHunk);
    host.dispatch(Action::ToggleApprove);
    assert!(
        host.model().status.starts_with("Approving"),
        "{}",
        host.model().status
    );
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleApprove);
    assert_eq!(host.model().saves.len(), 1, "one saves, one waits");
    host.settle();
    let view = host.model().view().unwrap();
    assert!(
        view.state(unit).unwrap().is_reviewed(),
        "{}",
        host.model().status
    );
    assert_eq!(run_lines(host.model()), Vec::<usize>::new());
}

/// A flag of the whole unit made while the approval of one of its changes saves shows on that
/// change too, as the tree shows it, since the flag is what the unit ends up with.
#[test]
fn a_flag_after_a_change_approval_shows_on_every_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    assert_eq!(host.model().hunk_badge(0), Some(rows::Badge::Accepted));
    host.dispatch(Action::ToggleFlag);
    let model = host.model();
    assert_eq!(model.hunk_badge(0), Some(rows::Badge::Flagged));
    assert_eq!(model.hunk_badge(1), Some(rows::Badge::Flagged));
}

/// Once every change of a file is on its way to approval, the keys that go on to a file that
/// needs the reviewer pass it by, although its meter still counts its unit open.
#[test]
fn a_file_whose_changes_are_all_saving_is_no_stop() {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = whole_file();
    settings.appearance.applied.cross_files = true;
    let edited = [10, 40];
    let mut host = accepted_below(
        dir.path(),
        "a.rs",
        "",
        &statements(&edited, "1"),
        &statements(&edited, "2"),
        settings,
    );
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::ToggleApprove);
    assert!(host.model().saving, "the approvals are still saving");
    host.update(|model| model.show_file("b.rs"));
    host.dispatch(Action::FocusDiff);
    assert_eq!(host.model().next_open_file(), None);
    host.dispatch(Action::PreviousOpenHunk);
    assert_eq!(host.model().selected.as_deref(), Some("b.rs"));
}

/// When the approval of a long change that only removes lines saves, the rows it took go, and
/// the change Space moved on to stays on screen, so the next Space approves it.
#[test]
fn a_saved_approval_keeps_the_next_change_on_screen() {
    use std::fmt::Write as _;

    let dir = tempfile::tempdir().unwrap();
    // Sixty statements go, and one far below them changes.
    let (mut long, mut second) = (String::from("fn a() {\n"), String::from("fn a() {\n"));
    for line in 0..160 {
        let value = usize::from(line == 150);
        writeln!(long, "    let v{line} = {value};").unwrap();
        if !(19..79).contains(&line) {
            writeln!(second, "    let v{line} = {};", value * 2).unwrap();
        }
    }
    long.push_str("}\n");
    second.push_str("}\n");
    let mut host = accepted_below(dir.path(), "a.rs", "", &long, &second, whole_file());
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    assert_eq!(
        host.model().hunk_rows.len(),
        2,
        "{:?}",
        run_lines(host.model())
    );
    host.dispatch(Action::ToggleApprove);
    assert_eq!(host.model().hunk, 1);
    assert!(host.model().hunk_in_view());
    host.settle();
    lay_out(&mut host);
    assert_eq!(host.model().hunk_rows.len(), 1);
    assert!(
        host.model().hunk_in_view(),
        "{}",
        host.model().diff_scroll.offset()
    );
}

/// Approving the last change of a long unit, which then shows whole as one hunk from its start,
/// keeps the code the reviewer reads where it is on screen.
#[test]
fn approving_a_units_last_change_keeps_the_code_on_screen() {
    use std::fmt::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let mut long = String::from("fn a() {\n");
    for line in 0..160 {
        writeln!(long, "    let v{line} = 0;").unwrap();
    }
    long.push_str("}\n");
    let second = long.replace("v150 = 0", "v150 = 2");
    let mut host = accepted_below(dir.path(), "a.rs", "", &long, &second, whole_file());
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    assert_eq!(run_lines(host.model()), [151]);
    let before = host.model().diff_scroll.target();
    assert!(before > 100.0, "the change is far down the unit: {before}");
    host.dispatch(Action::ToggleApprove);
    host.settle();
    lay_out(&mut host);
    assert_eq!(run_lines(host.model()), Vec::<usize>::new());
    let after = host.model().diff_scroll.target();
    assert!((after - before).abs() < 2.0, "{before} → {after}");
}

/// Space on a unit the reviewer folded never approves its changes since review unseen: it
/// unfolds the unit at its first change.
#[test]
fn space_on_a_folded_unit_shows_its_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleFold);
    assert_eq!(
        run_lines(host.model()),
        Vec::<usize>::new(),
        "one folded hunk"
    );
    host.dispatch(Action::ToggleApprove);
    assert!(host.model().saves.is_empty() && !host.model().saving);
    assert_eq!(run_lines(host.model()), [11, 41]);
    let model = host.model();
    assert_eq!(model.rows.hunks[model.hunk].at(), Some(11));
    assert!(model.status.starts_with("Unfolded"), "{}", model.status);
}

/// The selected change is kept as itself, not by its place among the unit's changes: when
/// another window approves the change before it, the same change stays selected, and when it
/// approves the selected one, the next is selected, which a held Space never acts on and a
/// press does, as a terminal's every key is.
#[test]
fn the_selected_change_is_kept_by_where_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 30, 50]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    let unit = host.model().rows.hunks[0].unit;
    let selected = |host: &Host| host.model().rows.hunks[host.model().hunk].at();
    assert_eq!(selected(&host), Some(31));
    let review = host.review.clone();
    let approve_elsewhere = |index: usize| {
        let history = review.history().unwrap();
        review
            .approve_run(
                &history.view(unit.revision()).unwrap(),
                &shown_change(&review, unit, index),
            )
            .unwrap();
    };

    approve_elsewhere(0);
    host.dispatch(Action::Refresh);
    host.settle();
    assert_eq!(run_lines(host.model()), [31, 51]);
    assert_eq!(selected(&host), Some(31), "the same change");

    approve_elsewhere(0);
    host.key(Action::ToggleApprove, false);
    // The other window's approval shows as a change on disk would, with no key between.
    host.update(Model::refresh);
    host.settle();
    assert_eq!(selected(&host), Some(51), "the next change");
    // The press that was saving is refused; the key still held never acts on the change now
    // selected.
    host.key(Action::ToggleApprove, true);
    assert!(host.model().saves.is_empty() && !host.model().saving);
    assert!(
        host.model().status.contains("held down"),
        "{}",
        host.model().status
    );
    host.key(Action::ToggleApprove, false);
    host.settle();
    assert_eq!(run_lines(host.model()), Vec::<usize>::new(), "a new press");
}

/// A key held down across the approval of a change approves that change alone, where the
/// front end tells repeats: the repeats that come while it saves wait, and once the diff moved
/// on to the next change, never act on it.
#[test]
fn held_repeats_across_an_approval_approve_only_the_first_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.key(Action::ToggleApprove, false);
    for _ in 0..5 {
        host.key(Action::ToggleApprove, true);
    }
    host.settle();
    let model = host.model();
    assert_eq!(run_lines(model), [41]);
    assert_eq!(model.rows.hunks[model.hunk].at(), Some(41));
    assert!(model.saves.is_empty() && !model.saving);
    assert!(model.status.contains("held down"), "{}", model.status);
}

/// Quick taps of Space, as a terminal delivers every key, approve one change each, as many
/// as there are taps, before any of the approvals saves.
#[test]
fn quick_taps_approve_a_change_each() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 30, 50, 70]);
    host.dispatch(Action::FocusDiff);
    for _ in 0..4 {
        host.key(Action::ToggleApprove, false);
    }
    host.settle();
    assert_eq!(run_lines(host.model()), Vec::<usize>::new());
    assert!(
        !host.model().status.starts_with("Not marked"),
        "{}",
        host.model().status
    );
}

/// Space, `j` and Space typed before the first approval saves act on what each sees: the first
/// Space approves the first change and moves on to the second at once, `j` moves past it to
/// the third, and the second Space, a new press after another key, approves the third.
#[test]
fn space_j_space_before_a_save_approve_the_changes_each_sees() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 30, 50]);
    host.dispatch(Action::FocusDiff);
    let selected = |host: &Host| host.model().rows.hunks[host.model().hunk].at();
    assert_eq!(selected(&host), Some(11));
    host.key(Action::ToggleApprove, false);
    assert_eq!(selected(&host), Some(31));
    host.key(Action::NextHunk, false);
    assert_eq!(selected(&host), Some(51));
    host.key(Action::ToggleApprove, false);
    assert!(host.model().saving && host.model().saves.len() == 1);
    host.settle();
    assert_eq!(run_lines(host.model()), [31]);
    assert_eq!(selected(&host), Some(31), "the one change left");
}

/// A recapture that moves a unit along an unchanged link keeps its selected change as itself,
/// its lines moved with the unit, and a key held on it goes on acting on that change, never on
/// the change that came to stand where it stood.
#[test]
fn a_recapture_keeps_the_selected_change_as_itself() {
    let dir = tempfile::tempdir().unwrap();
    let header = "// header\n".repeat(10);
    let edited = [10, 30, 50];
    let mut host = accepted_below(
        dir.path(),
        "a.rs",
        &header,
        &statements(&edited, "1"),
        &statements(&edited, "2"),
        settings(),
    );
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    let selected = |host: &Host| host.model().rows.hunks[host.model().hunk].at();
    assert_eq!(selected(&host), Some(41));
    host.update(Model::hold);

    // Twenty lines that change nothing reviewed come above the header.
    let above = "// new\n".repeat(20);
    let repo = dir.path().join("repo");
    std::fs::write(
        repo.join("a.rs"),
        above + &header + &statements(&edited, "2"),
    )
    .unwrap();
    host.update(|model| model.capture(false));
    host.settle();
    assert_eq!(run_lines(host.model()), [41, 61, 81]);
    assert_eq!(selected(&host), Some(61), "the same change, moved");

    host.key(Action::ToggleApprove, true);
    host.settle();
    assert_eq!(
        run_lines(host.model()),
        [41, 81],
        "the held key approved it"
    );
    host.key(Action::ToggleApprove, true);
    host.settle();
    assert_eq!(run_lines(host.model()), [41, 81], "and nothing after it");
}

/// A press that reveals what it acts on, by unfolding a unit, showing a hunk scrolled away or
/// leaving the buffer for a change only the diff shows, never approves anything while it is
/// held.
#[test]
fn repeats_after_a_reveal_approve_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleFold);
    host.key(Action::ToggleApprove, false);
    assert!(host.model().status.starts_with("Unfolded"));
    host.key(Action::ToggleApprove, true);
    assert!(
        host.model().saves.is_empty() && !host.model().saving,
        "unfolded"
    );

    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(demo(dir.path()), whole_file());
    host.update(|model| model.show_file("server.go"));
    host.dispatch(Action::FocusDiff);
    lay_out(&mut host);
    host.update(|model| {
        model.drag_list_to(List::Diff, 1e6);
        model.hunk = 0;
    });
    host.key(Action::ToggleApprove, false);
    assert!(
        host.model().status.contains("off screen"),
        "{}",
        host.model().status
    );
    host.key(Action::ToggleApprove, true);
    assert!(
        host.model().saves.is_empty() && !host.model().saving,
        "scrolled to"
    );

    let dir = tempfile::tempdir().unwrap();
    let mut host = edits_and_a_removal(dir.path());
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleBuffer);
    host.key(Action::ToggleApprove, false);
    assert_eq!(host.model().mode, Mode::Diff);
    host.key(Action::ToggleApprove, true);
    assert!(
        host.model().saves.is_empty() && !host.model().saving,
        "left the buffer"
    );
}

/// Coming back to a file, and switching the view on a change that only removes lines, which
/// after a partial approval has no line of any file, keep the selected change rather than the
/// unit's first.
#[test]
fn the_selected_change_survives_leaving_the_file_and_switching_views() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = edits_and_a_removal(dir.path());
    host.dispatch(Action::FocusDiff);
    let selected = |host: &Host| host.model().rows.hunks[host.model().hunk].at();
    assert_eq!(run_lines(host.model()), [11, 26, 41]);
    // Approving the second edit leaves the first and the removal after it, which shows against
    // text in no file.
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert_eq!(run_lines(host.model()), [11, 41]);
    assert_eq!(selected(&host), Some(41));
    host.dispatch(Action::ToggleView);
    assert_eq!(selected(&host), Some(41));
    host.dispatch(Action::ToggleView);
    assert_eq!(selected(&host), Some(41));
    host.update(|model| model.show_file("b.rs"));
    host.update(|model| model.show_file("a.rs"));
    assert_eq!(selected(&host), Some(41));
}

/// The window of [`accepted_then`] where statements 10 and 25 changed and statement 40 was
/// removed: changes at lines 11 and 26, and a removal standing at line 41.
fn edits_and_a_removal(dir: &Path) -> Host {
    let first = statements(&[10, 25], "1");
    let second = statements(&[10, 25], "2").replace("    let v40 = 0;\n", "");
    accepted_then(dir, "a.rs", &first, &second)
}

/// Space in the buffer on a change that only removes lines, which the buffer cannot show,
/// shows it in the diff rather than approving it.
#[test]
fn space_on_a_removal_in_the_buffer_shows_it_in_the_diff() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = edits_and_a_removal(dir.path());
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleBuffer);
    assert_eq!(host.model().buffer.as_ref().unwrap().cursor.row, 41);
    host.dispatch(Action::ToggleApprove);
    assert!(host.model().saves.is_empty() && !host.model().saving);
    let model = host.model();
    assert_eq!(model.mode, Mode::Diff);
    assert_eq!(model.rows.hunks[model.hunk].at(), Some(41));
    assert!(
        model.status.starts_with("This change only removes"),
        "{}",
        model.status
    );
}

/// The buffer's gutter marks only the open changes of a unit that shows them apart, the lines
/// Space acts on; Space on a change that only removes lines shows it in the diff first.
#[test]
fn the_buffer_marks_only_the_open_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleBuffer);
    let marked: Vec<usize> = host
        .model()
        .buffer
        .as_ref()
        .unwrap()
        .markers
        .iter()
        .enumerate()
        .filter_map(|(row, marker)| marker.map(|_| row))
        .collect();
    assert_eq!(marked, [11, 41]);
}

/// The file header's meter counts units, so one unit of two changes since review reads 0/1
/// before and after its first change is approved, and 1/1 after its last; folding the unit
/// changes nothing.
#[test]
fn the_file_meter_counts_units_not_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = accepted_then_edited(dir.path(), "a.rs", &[10, 40]);
    host.dispatch(Action::FocusDiff);
    let meter = |host: &Host| {
        let model = host.model();
        let palette = crate::paint::Palette::of(model);
        let room = crate::paint::Room {
            budget: 400.0,
            cell: 1.0,
            gap: 1.0,
        };
        let header = crate::paint::header::file_header(model, &palette, room, 1.0).unwrap();
        header.meter.unwrap().counts.text.trim().to_owned()
    };
    assert_eq!(meter(&host), "0/1");
    host.dispatch(Action::ToggleFold);
    assert_eq!(meter(&host), "0/1", "folding counts the same");
    host.dispatch(Action::ToggleFold);
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert_eq!(meter(&host), "0/1");
    host.dispatch(Action::ToggleApprove);
    host.settle();
    assert_eq!(meter(&host), "1/1");
}

/// A press on a change whose approval is saving waits for it, even after an approval above it
/// saved meanwhile and moved where its reviewed lines are in the reviewed text: it is the same
/// change by the identity the engine compares, so it never queues an approval the engine
/// would refuse once the first lands.
#[test]
fn a_press_on_a_change_saving_waits_after_a_shift_above_it() {
    let dir = tempfile::tempdir().unwrap();
    let first = statements(&[40], "1");
    let inserted = ["    let v10 = 0;\n", "    let inserted = 0;\n"].concat();
    let second = statements(&[40], "2").replacen("    let v10 = 0;\n", &inserted, 1);
    let mut host = accepted_then(dir.path(), "a.rs", &first, &second);
    host.dispatch(Action::FocusDiff);
    let unit = host.model().rows.hunks[0].unit;
    assert_eq!(run_lines(host.model()), [12, 42]);

    // The insertion saves while the edit below it waits.
    host.dispatch(Action::ToggleApprove);
    host.dispatch(Action::NextHunk);
    host.dispatch(Action::ToggleApprove);
    let inserted = host.run_job();
    host.complete(inserted);
    assert!(host.model().saving, "the edit is saving now");

    // Space again on the edit, as the rows read after the insertion show it.
    host.dispatch(Action::ToggleApprove);
    assert!(host.model().saves.is_empty(), "nothing queued twice");
    host.settle();
    let view = host.model().view().unwrap();
    assert!(
        view.state(unit).unwrap().is_reviewed(),
        "{}",
        host.model().status
    );
}

/// `image` encoded as `format`, as a file's bytes.
fn image_file(image: &image::RgbaImage, format: image::ImageFormat) -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut bytes, format)
        .unwrap();
    bytes.into_inner()
}

/// A `width` × `height` image of one opaque color.
fn filled(width: u32, height: u32, color: [u8; 4]) -> image::RgbaImage {
    image::RgbaImage::from_pixel(width, height, image::Rgba(color))
}

/// The comparison `path` shows once its jobs ran, as paint draws it.
fn comparison_of(host: &mut Host, path: &str) -> Option<crate::paint::images::ComparisonPaint> {
    host.update(|model| model.show_file(path));
    host.settle();
    crate::paint::images::comparison(host.model(), &crate::paint::Palette::of(host.model()))
}

/// The text of `pieces`, as one line.
fn line_of(pieces: &[crate::paint::Piece]) -> String {
    pieces.iter().map(|piece| piece.text.as_str()).collect()
}

/// A changed image is one unit, as any binary file, and shows both versions with how many
/// pixels changed; the toggle shows the head with them tinted, and hides them again.
#[test]
fn a_changed_image_compares_its_versions() {
    let dir = tempfile::tempdir().unwrap();
    let base = filled(4, 3, [200, 30, 30, 255]);
    let mut head = base.clone();
    head.put_pixel(0, 0, image::Rgba([30, 200, 30, 255]));
    head.put_pixel(3, 2, image::Rgba([0, 0, 0, 0]));
    let review = review_of(
        dir.path(),
        &[("logo.png", image_file(&base, image::ImageFormat::Png))],
        &[("logo.png", image_file(&head, image::ImageFormat::Png))],
    );
    let mut host = Host::open(review);
    let shown = comparison_of(&mut host, "logo.png").expect("the images compare");
    assert_eq!(
        host.model().view().unwrap().units_of_file("logo.png").len(),
        1
    );

    // Both versions, labeled with their format and size, the head drawn plain at first.
    assert_eq!(shown.base.name.text, "base");
    assert_eq!(shown.base.detail.text, "PNG · 4 × 3");
    assert_eq!(shown.head.detail.text, "PNG · 4 × 3");
    assert!(shown.base.note.is_none() && shown.head.note.is_none());
    assert!(!shown.tinted);
    assert_eq!(
        line_of(&shown.status),
        "2 of 12 pixels changed (16.6%) · o tints them"
    );
    let plain = shown.head.raster.clone().unwrap();
    assert_eq!(*plain, head);
    // The file header says what the file is.
    let palette = crate::paint::Palette::of(host.model());
    let room = crate::paint::Room {
        budget: 200.0,
        cell: 1.0,
        gap: 1.0,
    };
    let header = crate::paint::header::file_header(host.model(), &palette, room, 1.0).unwrap();
    assert_eq!(header.details.text, "PNG");

    // The toggle draws the head with its changed pixels tinted, and draws it plain again.
    host.dispatch(Action::ToggleChangedPixels);
    let tinted =
        crate::paint::images::comparison(host.model(), &crate::paint::Palette::of(host.model()))
            .unwrap();
    assert!(tinted.tinted);
    assert_ne!(tinted.head.raster.as_deref(), Some(&head));
    assert!(line_of(&tinted.status).ends_with("o hides them"));
    host.dispatch(Action::ToggleChangedPixels);
    assert!(!comparison_of(&mut host, "logo.png").unwrap().tinted);
}

/// Images of other sizes, and images whose bytes changed but whose pixels did not, say so and
/// refuse the toggle; an added image says it has no base and a deleted one no head; a binary
/// file that is no image shows no comparison.
#[test]
fn images_say_what_they_cannot_compare() {
    let dir = tempfile::tempdir().unwrap();
    let small = filled(2, 2, [10, 10, 10, 255]);
    let wide = filled(3, 2, [10, 10, 10, 255]);
    let png = |image: &image::RgbaImage| image_file(image, image::ImageFormat::Png);
    let review = review_of(
        dir.path(),
        &[
            ("data.bin", b"DATA\0one".to_vec()),
            // Other content than new.png's, which git would take for a rename of it.
            ("gone.png", png(&wide)),
            ("same.png", png(&small)),
            ("wide.png", png(&small)),
        ],
        &[
            ("data.bin", b"DATA\0two".to_vec()),
            ("new.png", png(&small)),
            ("same.png", image_file(&small, image::ImageFormat::Bmp)),
            ("wide.png", png(&wide)),
        ],
    );
    let mut host = Host::open(review);

    let resized = comparison_of(&mut host, "wide.png").unwrap();
    assert_eq!(resized.head.detail.text, "PNG · 3 × 2");
    assert_eq!(
        line_of(&resized.status),
        "the sizes differ, so no pixels are compared"
    );
    host.dispatch(Action::ToggleChangedPixels);
    assert_eq!(
        host.model().status,
        "The versions differ in size, so no pixels are compared."
    );
    assert!(!host.model().changed_pixels_shown());

    let same = comparison_of(&mut host, "same.png").unwrap();
    assert_eq!(same.head.detail.text, "BMP · 2 × 2");
    assert_eq!(
        line_of(&same.status),
        "the decoded images show no pixel differences"
    );

    let added = comparison_of(&mut host, "new.png").unwrap();
    assert_eq!(
        added.base.note.map(|note| note.text).as_deref(),
        Some("none: the file is new")
    );
    assert!(added.base.raster.is_none());
    assert_eq!(line_of(&added.status), "");
    let deleted = comparison_of(&mut host, "gone.png").unwrap();
    assert_eq!(
        deleted.head.note.map(|note| note.text).as_deref(),
        Some("none: the file is deleted")
    );
    assert_eq!(deleted.base.detail.text, "PNG · 3 × 2");

    assert!(comparison_of(&mut host, "data.bin").is_none());
}

/// Nothing large is read or decoded: a blob past the ceiling stays unread and an image with
/// too many pixels undecoded, each saying so beside the other version.
#[test]
fn large_versions_are_refused_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let small = image_file(&filled(2, 2, [1, 2, 3, 255]), image::ImageFormat::Png);
    // A PNG's signature, then zeros past the ceiling.
    let mut oversized = small[..8].to_vec();
    oversized.resize(usize::try_from(crate::images::MAX_BYTES).unwrap() + 1, 0);
    // A BMP's headers, claiming 5000 × 5000 pixels of 24 bits, with no pixels after them.
    let mut huge = b"BM".to_vec();
    for field in [54_u32, 0, 54, 40, 5000, 5000] {
        huge.extend(field.to_le_bytes());
    }
    huge.extend(1_u16.to_le_bytes());
    huge.extend(24_u16.to_le_bytes());
    huge.extend([0; 24]);
    let review = review_of(
        dir.path(),
        &[("huge.bmp", small.clone()), ("large.png", small.clone())],
        &[("huge.bmp", huge), ("large.png", oversized)],
    );
    let mut host = Host::open(review);

    let large = comparison_of(&mut host, "large.png").unwrap();
    assert_eq!(large.head.detail.text, "8.3 MB");
    assert_eq!(
        large.head.note.map(|note| note.text).as_deref(),
        Some("too large to preview")
    );
    assert!(large.base.raster.is_some());

    let huge = comparison_of(&mut host, "huge.bmp").unwrap();
    assert_eq!(huge.head.detail.text, "BMP · 5000 × 5000");
    assert_eq!(
        huge.head.note.map(|note| note.text).as_deref(),
        Some("too many pixels to preview")
    );
}

/// One comparison runs at a time and lands only for the file that shows: moving on while one
/// runs drops what it found, and the file shown then is compared next.
#[test]
fn only_the_shown_file_takes_its_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let png = |shade| {
        image_file(
            &filled(2, 2, [shade, shade, shade, 255]),
            image::ImageFormat::Png,
        )
    };
    let review = review_of(
        dir.path(),
        &[("a.png", png(1)), ("b.png", png(2)), ("c.png", png(3))],
        &[("a.png", png(4)), ("b.png", png(5)), ("c.png", png(6))],
    );
    let mut host = Host::open(review);
    let pair_of = |host: &Host| {
        host.model()
            .comparison()
            .map(|compared| compared.pair.clone())
    };
    assert!(
        comparison_of(&mut host, "a.png").is_some(),
        "a.png compares"
    );
    let a = pair_of(&host);

    // b.png's comparison starts, in the job that also finds it is no PDF, and a.png's no
    // longer shows under it.
    host.update(|model| model.show_file("b.png"));
    let busy = |host: &Host| host.model().images.busy() || host.model().pdfs.busy();
    assert!(busy(&host));
    assert_eq!(pair_of(&host), None);
    // c.png shows while b.png's job runs, which no second job joins.
    host.update(|model| model.show_file("c.png"));
    let waiting: Vec<Completion> = host.jobs.drain(..).map(Job::run).collect();
    let comparing = waiting
        .iter()
        .filter(|completion| {
            matches!(
                *completion.outcome,
                Outcome::Compared(_) | Outcome::PdfRead(_)
            )
        })
        .count();
    assert_eq!(comparing, 1, "one comparison at a time");
    // b.png's comparison lands for a file no longer shown, and never shows under c.png.
    for completion in waiting {
        host.complete(completion);
        assert_eq!(pair_of(&host), None);
    }
    host.settle();
    let c = pair_of(&host).expect("c.png compares");
    assert_ne!(Some(&c), a.as_ref());
    assert!(!busy(&host));

    // The one comparison kept is c.png's: a.png compares again.
    host.update(|model| model.show_file("a.png"));
    assert_eq!(pair_of(&host), None);
    host.settle();
    assert_eq!(pair_of(&host), a);
}

/// A still image that became an animation with the same first frame: only first frames are
/// compared, which each label and the line under them say; nothing claims the versions are
/// alike.
#[test]
fn an_animation_says_what_was_compared() {
    use image::codecs::gif::GifEncoder;

    let gif = |colors: &[[u8; 4]]| {
        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for &color in colors {
                let delay =
                    image::Delay::from_saturating_duration(std::time::Duration::from_millis(500));
                let frame = image::Frame::from_parts(filled(4, 3, color), 0, 0, delay);
                encoder.encode_frame(frame).unwrap();
            }
        }
        bytes
    };
    let sky = [142, 202, 230, 255];
    let dir = tempfile::tempdir().unwrap();
    let review = review_of(
        dir.path(),
        &[("spinner.gif", gif(&[sky]))],
        &[("spinner.gif", gif(&[sky, [33, 158, 188, 255]]))],
    );
    let mut host = Host::open(review);
    let shown = comparison_of(&mut host, "spinner.gif").unwrap();
    assert_eq!(shown.base.detail.text, "GIF · 4 × 3 · first frame");
    assert_eq!(shown.head.detail.text, "GIF · 4 × 3 · first frame");
    assert_eq!(
        line_of(&shown.status),
        "the first frames show no pixel differences"
    );
    host.dispatch(Action::ToggleChangedPixels);
    assert_eq!(
        host.model().status,
        "The compared images show no pixel differences."
    );
}

/// A front end that shows no images, as a terminal without an image protocol, has nothing
/// read or decoded for a changed image, and the toggle says why; once it shows images, the
/// selected image is compared.
#[test]
fn nothing_is_decoded_where_no_image_shows() {
    let dir = tempfile::tempdir().unwrap();
    let png = |shade| {
        image_file(
            &filled(2, 2, [shade, shade, shade, 255]),
            image::ImageFormat::Png,
        )
    };
    let review = review_of(dir.path(), &[("a.png", png(1))], &[("a.png", png(2))]);
    let terminal = Settings {
        front_end: crate::front_end::FrontEnd::Terminal,
        ..settings()
    };
    let mut host = Host::open_with(review, terminal);
    host.update(|model| model.show_file("a.png"));
    let comparing = |host: &mut Host| {
        let completions: Vec<Completion> = host.jobs.drain(..).map(Job::run).collect();
        let compared = completions
            .iter()
            .filter(|completion| matches!(*completion.outcome, Outcome::Compared(_)))
            .count();
        for completion in completions {
            host.complete(completion);
        }
        compared
    };
    assert_eq!(comparing(&mut host), 0, "an image was read");
    host.settle();
    assert!(host.model().comparison().is_none());
    host.dispatch(Action::ToggleChangedPixels);
    assert_eq!(
        host.model().status,
        "This terminal shows no images, so a changed image shows as a binary file."
    );

    host.update(|model| model.show_images(true));
    assert_eq!(comparing(&mut host), 1);
    host.settle();
    assert!(host.model().comparison().is_some());
}

/// Moving from one image to another, the new one's comparison is pending from the first frame:
/// its place shows both labels and nothing of the other file's images, says it is decoding
/// only once its timer ran, and shows the new file's images once its job lands.
#[test]
fn a_pending_comparison_keeps_its_place_and_shows_no_other_image() {
    let dir = tempfile::tempdir().unwrap();
    let png = |shade| {
        image_file(
            &filled(2, 2, [shade, shade, shade, 255]),
            image::ImageFormat::Png,
        )
    };
    let review = review_of(
        dir.path(),
        &[("a.png", png(1)), ("b.png", png(3))],
        &[("a.png", png(2)), ("b.png", png(4))],
    );
    let mut host = Host::open(review);
    let paint = |host: &Host| {
        crate::paint::images::comparison(host.model(), &crate::paint::Palette::of(host.model()))
    };
    let shade = |version: &crate::paint::images::VersionPaint| {
        version
            .raster
            .as_ref()
            .map(|raster| raster.get_pixel(0, 0).0[0])
    };
    assert!(comparison_of(&mut host, "a.png").is_some());

    host.update(|model| model.show_file("b.png"));
    let pending = paint(&host).expect("the comparison keeps its place");
    assert_eq!(pending.base.name.text, "base");
    assert_eq!(pending.head.name.text, "head");
    assert_eq!((shade(&pending.base), shade(&pending.head)), (None, None));
    assert_eq!(
        pending.base.note, None,
        "a quick comparison never says it is decoding"
    );

    let timer = host
        .timers
        .iter()
        .position(|timer| {
            matches!(
                *timer.outcome,
                Outcome::Tick(super::sync::Tick::Decoding(_))
            )
        })
        .expect("the comparison's grace runs");
    host.fire(timer);
    let slow = paint(&host).expect("the comparison keeps its place");
    assert_eq!(
        slow.base.note.as_ref().map(|note| note.text.as_str()),
        Some("decoding…")
    );
    assert_eq!((shade(&slow.base), shade(&slow.head)), (None, None));

    host.settle();
    let landed = paint(&host).expect("b.png compares");
    assert_eq!(
        (shade(&landed.base), shade(&landed.head)),
        (Some(3), Some(4))
    );
}

/// The rows of the shown diff as their texts, and each header's tag after it.
fn row_texts(host: &Host) -> Vec<String> {
    host.model()
        .rows
        .rows
        .iter()
        .map(|row| match &row.body {
            rows::Body::Note(note) => match &note.tag {
                Some(tag) => format!("{} [{tag}]", note.text),
                None => note.text.clone(),
            },
            _ => row.text().to_owned(),
        })
        .collect()
}

/// The page pair the shown comparison renders, one-based.
#[cfg(feature = "pdf")]
fn compared_page(host: &Host) -> Option<(Option<u32>, Option<u32>)> {
    let page = host.model().comparison()?.pair.page?;
    Some((
        page.base.map(|page| page + 1),
        page.head.map(|page| page + 1),
    ))
}

/// A changed PDF, one with a page whose text changed and an inserted page.
#[cfg(feature = "pdf")]
fn pdf_review(dir: &Path) -> Review {
    use crate::pdf::testing::pdf;
    review_of(
        dir,
        &[("doc.pdf", pdf(&["Cover", "Terms one", "Middle", "End"]))],
        &[(
            "doc.pdf",
            pdf(&["Cover", "Terms two", "Middle", "Inserted", "End"]),
        )],
    )
}

/// A changed PDF is one unit that approves the whole PDF, shows the text diff of its first
/// changed page pair under a navigator, and renders that pair for the comparison; the
/// navigator steps through every page, and a page only the head has says so.
#[cfg(feature = "pdf")]
#[test]
fn a_changed_pdf_shows_its_pages() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(pdf_review(dir.path()));
    host.update(|model| model.show_file("doc.pdf"));
    host.settle();
    assert_eq!(
        host.model().view().unwrap().units_of_file("doc.pdf").len(),
        1
    );
    let texts = row_texts(&host);
    assert!(
        texts.iter().any(|text| text.starts_with("PDF")),
        "{texts:#?}"
    );
    assert!(
        texts.contains(
            &"Base 2/4 → Head 2/5 · text changed · } next page · { previous · P hides the pages"
                .to_owned()
        ),
        "{texts:#?}"
    );
    // The changed line once, its old word struck out before its new one.
    assert!(texts.contains(&"Terms onetwo".to_owned()), "{texts:#?}");
    // The file header says what the file is.
    let palette = crate::paint::Palette::of(host.model());
    let room = crate::paint::Room {
        budget: 200.0,
        cell: 1.0,
        gap: 1.0,
    };
    let header = crate::paint::header::file_header(host.model(), &palette, room, 1.0).unwrap();
    assert_eq!(header.details.text, "PDF");
    // The page pair renders above the text.
    assert_eq!(compared_page(&host), Some((Some(2), Some(2))));
    let shown =
        crate::paint::images::comparison(host.model(), &crate::paint::Palette::of(host.model()))
            .unwrap();
    assert_eq!(shown.head.detail.text, "PDF page 2 · 200 × 280 pt");
    assert!(
        line_of(&shown.status).contains("pixels changed"),
        "{:?}",
        line_of(&shown.status)
    );

    for _ in 0..2 {
        host.dispatch(Action::NextPage);
    }
    host.settle();
    assert_eq!(
        host.model().page_navigator().as_deref(),
        Some("Head 4/5 · added page")
    );
    assert_eq!(compared_page(&host), Some((None, Some(4))));
    let shown =
        crate::paint::images::comparison(host.model(), &crate::paint::Palette::of(host.model()))
            .unwrap();
    assert_eq!(
        shown.base.note.as_ref().map(|note| note.text.as_str()),
        Some("none: the page is new")
    );
    host.dispatch(Action::NextPage);
    host.dispatch(Action::NextPage);
    assert_eq!(host.model().status, "This is the last page.");

    // In the diff, the PDF's one unit is the whole PDF, whatever page shows.
    host.dispatch(Action::FocusDiff);
    assert!(host.model().approves_pdf());
    host.dispatch(Action::ToggleApprove);
    host.settle();
    let view = host.model().view().unwrap();
    let unit = view.units_of_file("doc.pdf")[0];
    assert!(view.standing(unit).unwrap().is_approved());
}

/// Pages switched fast while a page renders never show a page late: the render for a page left
/// lands under no other page, and the page shown now renders next.
#[cfg(feature = "pdf")]
#[test]
fn fast_page_switches_show_no_stale_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(pdf_review(dir.path()));
    host.update(|model| model.show_file("doc.pdf"));
    host.settle();
    assert_eq!(compared_page(&host), Some((Some(2), Some(2))));

    host.dispatch(Action::NextPage);
    let rendering = host.run_job();
    host.dispatch(Action::NextPage);
    host.dispatch(Action::NextPage);
    assert!(host.jobs.is_empty(), "a second render joined the first");
    host.complete(rendering);
    assert_eq!(
        compared_page(&host),
        None,
        "page 3's render showed under another"
    );
    host.settle();
    assert_eq!(compared_page(&host), Some((Some(4), Some(5))));
    assert_eq!(
        host.model().page_navigator().as_deref(),
        Some("Base 4/4 → Head 5/5 · extracted text identical")
    );
}

/// An encrypted PDF and one too large to read show as binary files whose header says why,
/// with no pages and no comparison.
#[cfg(feature = "pdf")]
#[test]
fn unreadable_pdfs_say_why_on_their_binary_line() {
    use crate::pdf::testing::{encrypted, pdf};
    let dir = tempfile::tempdir().unwrap();
    let mut large = b"%PDF-1.4\n".to_vec();
    large.resize(usize::try_from(crate::images::MAX_BYTES).unwrap() + 1, b'%');
    let review = review_of(
        dir.path(),
        &[
            ("locked.pdf", pdf(&["open"])),
            ("large.pdf", pdf(&["small"])),
        ],
        &[
            ("locked.pdf", encrypted(&["closed"])),
            ("large.pdf", large.clone()),
            // One new blob too large to read, named a PDF and not.
            ("big.pdf", large.clone()),
            ("big.bin", large),
        ],
    );
    let mut host = Host::open(review);
    for (path, why) in [
        ("locked.pdf", "binary content changed [head is encrypted]"),
        ("large.pdf", "[head too large to read (8.3 MB)]"),
    ] {
        host.update(|model| model.show_file(path));
        host.settle();
        let texts = row_texts(&host);
        assert!(texts.iter().any(|text| text.ends_with(why)), "{texts:#?}");
        assert_eq!(host.model().page_navigator(), None);
        assert!(host.model().comparison().is_none());
    }
    // Only its name makes a blob too large to read a PDF: read once for both paths, it says
    // so under the PDF's name alone.
    for (path, pdf) in [("big.pdf", true), ("big.bin", false), ("big.pdf", true)] {
        host.update(|model| model.show_file(path));
        host.settle();
        let texts = row_texts(&host);
        let says = texts
            .iter()
            .any(|text| text.ends_with("[head too large to read (8.3 MB)]"));
        assert_eq!(says, pdf, "{path}: {texts:#?}");
    }
}

/// Built without the `pdf` feature, a changed PDF is a binary file as any other.
#[cfg(not(feature = "pdf"))]
#[test]
fn without_pdf_support_a_pdf_is_a_binary_file() {
    let dir = tempfile::tempdir().unwrap();
    let review = review_of(
        dir.path(),
        &[("doc.pdf", b"%PDF-1.4\n% one\n".as_slice())],
        &[("doc.pdf", b"%PDF-1.4\n% two\n".as_slice())],
    );
    let mut host = Host::open(review);
    host.update(|model| model.show_file("doc.pdf"));
    host.settle();
    assert_eq!(host.model().page_navigator(), None);
    let texts = row_texts(&host);
    assert!(
        texts.contains(&"binary content changed".to_owned()),
        "{texts:#?}"
    );
    host.dispatch(Action::NextPage);
    assert_eq!(
        host.model().status,
        "Only a changed PDF has pages to step through."
    );
}

/// A folder's diff shows a PDF's unit without the page rows of its file's own diff, which
/// belong to the pair that diff read.
#[cfg(feature = "pdf")]
#[test]
fn a_folder_shows_no_page_rows() {
    use crate::pdf::testing::pdf;
    let dir = tempfile::tempdir().unwrap();
    let review = review_of(
        dir.path(),
        &[
            ("docs/doc.pdf", pdf(&["One", "Old"])),
            ("docs/a.txt", b"a\n".to_vec()),
        ],
        &[
            ("docs/doc.pdf", pdf(&["One", "New"])),
            ("docs/a.txt", b"b\n".to_vec()),
        ],
    );
    let mut host = Host::open(review);
    host.update(|model| model.show_file("docs/doc.pdf"));
    host.settle();
    assert!(
        row_texts(&host)
            .iter()
            .any(|text| text.starts_with("Base 2/2"))
    );
    let docs = folder_at(host.model(), "docs");
    host.update(|model| model.tree_move_to(docs));
    host.settle();
    assert_eq!(host.model().folder(), Some("docs"));
    let texts = row_texts(&host);
    assert!(
        !texts
            .iter()
            .any(|text| text.starts_with("Base") || text == "New"),
        "{texts:#?}"
    );
}

/// A PDF's rendered pages show until the reviewer hides them, and nothing renders while they
/// are hidden; the choice holds for every PDF, and an image shows its comparison whatever it
/// is, from the read that found it is no PDF.
#[cfg(feature = "pdf")]
#[test]
fn a_pdf_renders_its_pages_until_they_are_hidden() {
    use crate::pdf::testing::pdf;
    let dir = tempfile::tempdir().unwrap();
    let png = |shade| {
        image_file(
            &filled(2, 2, [shade, shade, shade, 255]),
            image::ImageFormat::Png,
        )
    };
    let review = review_of(
        dir.path(),
        &[
            ("a.pdf", pdf(&["One", "Two"])),
            ("b.pdf", pdf(&["Three"])),
            ("c.png", png(1)),
        ],
        &[
            ("a.pdf", pdf(&["One", "2"])),
            ("b.pdf", pdf(&["3"])),
            ("c.png", png(2)),
        ],
    );
    let mut host = Host::open(review);
    let renders = |host: &mut Host| {
        let completions: Vec<Completion> = host.jobs.drain(..).map(Job::run).collect();
        let rendered = completions
            .iter()
            .filter(|completion| matches!(*completion.outcome, Outcome::Compared(_)))
            .count();
        for completion in completions {
            host.complete(completion);
        }
        rendered
    };
    host.update(|model| model.show_file("a.pdf"));
    host.settle();
    assert_eq!(compared_page(&host), Some((Some(2), Some(2))));

    host.dispatch(Action::TogglePages);
    assert!(host.model().comparison().is_none());
    assert!(host.model().pending_comparison().is_none());
    host.dispatch(Action::PreviousPage);
    assert_eq!(renders(&mut host), 0, "a page rendered while hidden");
    // The choice holds for another PDF, which shows its text alone too.
    host.update(|model| model.show_file("b.pdf"));
    for _ in 0..3 {
        assert_eq!(renders(&mut host), 0, "a page rendered while hidden");
    }
    host.dispatch(Action::TogglePages);
    host.settle();
    assert_eq!(compared_page(&host), Some((Some(1), Some(1))));
    // An image shows its comparison, decoded by the one read of its blobs.
    host.dispatch(Action::TogglePages);
    host.update(|model| model.show_file("c.png"));
    assert_eq!(renders(&mut host), 0, "the image was read twice");
    host.settle();
    assert!(host.model().comparison().is_some());
}

/// Coming back to an image after a PDF reads the image again to learn it is no PDF, but keeps
/// the comparison it has rather than decoding it again.
#[cfg(feature = "pdf")]
#[test]
fn an_image_compared_before_is_not_decoded_again() {
    use crate::pdf::testing::pdf;
    let dir = tempfile::tempdir().unwrap();
    let png = |shade| {
        image_file(
            &filled(2, 2, [shade, shade, shade, 255]),
            image::ImageFormat::Png,
        )
    };
    let review = review_of(
        dir.path(),
        &[("a.png", png(1)), ("b.pdf", pdf(&["One"]))],
        &[("a.png", png(2)), ("b.pdf", pdf(&["Two"]))],
    );
    let mut host = Host::open(review);
    assert!(comparison_of(&mut host, "a.png").is_some());
    // A PDF read as its text alone: its pages hidden before their render lands.
    host.update(|model| model.show_file("b.pdf"));
    let read = host.run_job();
    host.complete(read);
    host.dispatch(Action::TogglePages);
    host.settle();
    host.update(|model| model.show_file("a.png"));
    let read = host.run_job();
    let Outcome::PdfRead(Some(found)) = &*read.outcome else {
        panic!("no read of a.png");
    };
    assert!(found.comparison.is_none(), "a.png was decoded again");
    host.complete(read);
    host.settle();
    assert!(host.model().comparison().is_some());
}

/// A checkout whose `a.rs` and `b.rs` changed from `main`, recorded twice by its review:
/// revision 1 with `a.rs` accepted and a note on `b.rs`, revision 2 with both statements of
/// `a.rs` changed again and the first change approved on its own; and the window over it
/// opening as the model's opener opens it, whose opening job waits to run.
/// With `stale`, the cache holds no analysis or correspondence, as after the format changed.
/// Returns it with the working tree.
fn recorded_checkout(dir: &Path, stale: bool) -> (Host, std::path::PathBuf) {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(repo.join("b.rs"), "fn b() {}\n").unwrap();
    git_in(&repo, &["add", "--all"]);
    git_in(&repo, &["commit", "--quiet", "-m", "base"]);
    git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let data = dir.join("data");
    let review = Review::open_checkout(&data, review_core::Repo::discover(&repo).unwrap()).unwrap();
    std::fs::write(repo.join("a.rs"), statements(&[10, 40], "1")).unwrap();
    std::fs::write(repo.join("b.rs"), "fn b() { b(); }\n").unwrap();
    review.refresh().unwrap();
    let history = review.history().unwrap();
    let view = history.current_view().unwrap();
    review
        .mark(&view, &view.units_of_file("a.rs"), MarkVerdict::Accepted)
        .unwrap();
    review
        .note(&view, view.units_of_file("b.rs")[0], "why?")
        .unwrap();
    std::fs::write(repo.join("a.rs"), statements(&[10, 40], "2")).unwrap();
    review.refresh().unwrap();
    let history = review.history().unwrap();
    let view = history.current_view().unwrap();
    let unit = view.units_of_file("a.rs")[0];
    review
        .approve_run(&view, &shown_change(&review, unit, 0))
        .unwrap();
    if stale {
        std::fs::remove_dir_all(review.dir().root().join("analysis")).unwrap();
    }
    drop(review);
    let loaded =
        crate::launch::open_checkout(&data, review_core::Repo::discover(&repo).unwrap()).unwrap();
    (Host::opening(loaded, settings()), repo)
}

/// Runs the waiting jobs in order, landing each, until one hands back what `wanted` picks,
/// whose completion it returns unapplied.
fn run_until(host: &mut Host, wanted: impl Fn(&Outcome) -> bool) -> Completion {
    loop {
        let completion = host.run_job();
        if wanted(&completion.outcome) {
            return completion;
        }
        host.complete(completion);
    }
}

/// Opens the review `host` opens and draws the frame that shows it.
fn show_opened(host: &mut Host) {
    let opened = run_until(host, |outcome| matches!(outcome, Outcome::Opened(_)));
    host.complete(opened);
    host.draw();
}

/// Every event in the log of `review`.
fn logged(review: &Review) -> Vec<review_core::store::events::Event> {
    review_core::store::log::LogTail::new(review.dir())
        .poll()
        .unwrap()
}

/// The checkout's review shows its last recorded revision at once, with its changes from the
/// cache, or after the format changed, its changed files until their analysis lands; the
/// reviewer moves between files while the review state is checked, and nothing counts.
#[test]
fn the_last_recorded_revision_shows_while_the_review_is_checked() {
    for stale in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, _repo) = recorded_checkout(dir.path(), stale);
        // The panes show empty while the review opens, with no card in the diff's place.
        let model = host.model();
        assert!(matches!(model.phase, Phase::Loading));
        assert!(model.sidebar_shown());
        assert_eq!(card(model), None);
        assert_eq!(crate::paint::header::no_file(model), "");
        show_opened(&mut host);
        let model = host.model();
        // Until the changes are analyzed, the diff stays empty, with no card either.
        assert_eq!(card(model), None, "stale: {stale}");
        let checking = model.checking().expect("the pending view shows");
        assert_eq!(checking.record.number.get(), 2);
        assert_eq!(checking.changes, !stale, "stale: {stale}");
        assert!(model.snapshot.is_none());
        assert_eq!(file_paths(&model.tree), ["a.rs", "b.rs"]);
        assert_eq!(model.totals(), (0, 0), "nothing counts");
        assert!(model.busy(), "the review is checked behind it");

        host.update(|model| model.show_file("b.rs"));
        assert_eq!(host.model().selected.as_deref(), Some("b.rs"));
        if stale {
            assert!(host.model().rows.hunks.is_empty(), "nothing to lay out yet");
            let analyzed = run_until(&mut host, |outcome| matches!(outcome, Outcome::Analyzed(_)));
            host.complete(analyzed);
        }
        host.draw();
        // b.rs lays out with the frame after its text, which the pending view did not bring, is
        // read.
        let read = run_until(&mut host, |outcome| {
            matches!(outcome, Outcome::TextsRead(_))
        });
        host.complete(read);
        host.draw();
        let model = host.model();
        assert_eq!(model.selected.as_deref(), Some("b.rs"));
        assert_eq!(model.rows.hunks.len(), 1, "stale: {stale}");
        assert_eq!(model.hunk_badge(0), None, "no unit stands anywhere");
        assert!(model.checking().is_some_and(|checking| checking.changes));
    }
}

/// The analysis of the last recorded revision shows only with the next frame, keeping the file
/// the reviewer moved to, so a key handled before that frame acts on the files the last frame
/// showed.
#[test]
fn a_late_analysis_shows_with_the_next_frame() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), true);
    show_opened(&mut host);
    let analyzed = run_until(&mut host, |outcome| matches!(outcome, Outcome::Analyzed(_)));
    host.update(|model| model.show_file("b.rs"));
    host.complete(analyzed);
    assert!(host.model().publishes());
    host.dispatch(Action::FocusDiff);
    assert!(
        host.model().rows.hunks.is_empty(),
        "the changes showed before a frame"
    );

    host.draw();
    let model = host.model();
    assert_eq!(model.selected.as_deref(), Some("b.rs"));
    // The analysis brought the text of a.rs, selected when it was asked for, so b.rs waits for
    // its own, with no card meanwhile.
    assert!(model.rows.rows.is_empty(), "b.rs lays out without its text");
    assert_eq!(card(model), None);
    let read = run_until(&mut host, |outcome| {
        matches!(outcome, Outcome::TextsRead(_))
    });
    host.complete(read);
    host.draw();
    let model = host.model();
    assert_eq!(
        model.rows.files.first().map(|file| file.path.as_str()),
        Some("b.rs")
    );
    assert_eq!(model.rows.hunks.len(), 1);
}

/// Whether the diff shows line `line` of the head file.
fn shows_line(model: &Model, line: usize) -> bool {
    model.rows.rows.iter().any(|row| {
        row.cells()
            .any(|cell| cell.head.as_ref().is_some_and(|head| head.line == line))
    })
}

/// A file's diff lays out once, with its context: the file the pending view shows first comes
/// with its text, and a file selected before its text is read shows nothing, not even a card,
/// until it is, rather than its changes alone first.
#[test]
fn a_diff_lays_out_once_with_its_context() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(repo.join("a.rs"), statements(&[], "0")).unwrap();
    std::fs::write(repo.join("b.rs"), statements(&[], "0")).unwrap();
    git_in(&repo, &["add", "--all"]);
    git_in(&repo, &["commit", "--quiet", "-m", "base"]);
    git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let data = dir.path().join("data");
    let review = Review::open_checkout(&data, review_core::Repo::discover(&repo).unwrap()).unwrap();
    std::fs::write(repo.join("a.rs"), statements(&[10, 40], "1")).unwrap();
    std::fs::write(repo.join("b.rs"), statements(&[20], "1")).unwrap();
    review.refresh().unwrap();
    drop(review);
    let loaded =
        crate::launch::open_checkout(&data, review_core::Repo::discover(&repo).unwrap()).unwrap();
    let mut host = Host::opening(loaded, settings());

    // a.rs shows line 9, `let v7`, unchanged before its first change, with its first frame.
    show_opened(&mut host);
    let model = host.model();
    assert!(model.checking().is_some(), "the pending view shows");
    assert_eq!(model.selected.as_deref(), Some("a.rs"));
    assert!(shows_line(model, 9), "a.rs shows its changes alone");

    // b.rs, whose text is still being read, shows nothing until it is, then the whole file.
    host.update(|model| model.show_file("b.rs"));
    let model = host.model();
    assert!(model.rows.rows.is_empty(), "b.rs shows its changes alone");
    assert_eq!(card(model), None);
    let read = run_until(&mut host, |outcome| {
        matches!(outcome, Outcome::TextsRead(_))
    });
    host.complete(read);
    host.draw();
    assert!(shows_line(host.model(), 20), "b.rs shows its changes alone");
}

/// A file whose rows waited for its text shows them with the next frame that can: not one
/// drawn under the overview, so Esc and Space handled together after it act on nothing, as no
/// frame showed the file's diff.
#[test]
fn rows_that_waited_for_their_text_show_with_the_next_frame() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    // The review is verified while b.rs's text, asked for by the pending view, is still read.
    let verified = host.run_newest_job();
    assert!(matches!(*verified.outcome, Outcome::Verified(_)));
    host.complete(verified);
    host.draw();
    assert!(
        host.model().checking().is_none(),
        "the verified review shows"
    );
    host.update(|model| model.show_file("b.rs"));
    assert!(host.model().rows.rows.is_empty(), "b.rs waits for its text");
    host.dispatch(Action::ToggleOverview);
    assert_eq!(host.model().overlay, Some(Overlay::Overview));
    let events = std::fs::read(host.review.dir().events()).unwrap();

    // The text lands, and a frame draws under the overview, which keeps b.rs waiting.
    let read = run_until(&mut host, |outcome| {
        matches!(outcome, Outcome::TextsRead(_))
    });
    host.complete(read);
    assert!(!host.model().publishes(), "the overview holds b.rs back");
    host.draw();
    assert!(
        host.model().rows.rows.is_empty(),
        "b.rs laid out under the overview"
    );

    // Esc and Space come together, before the frame that shows b.rs.
    host.key(Action::CloseOverlay, false);
    host.key(Action::ToggleApprove, false);
    assert!(host.model().saves.is_empty() && host.model().pending.is_empty());
    host.settle();
    assert_eq!(
        std::fs::read(host.review.dir().events()).unwrap(),
        events,
        "a key marked a diff no frame showed"
    );

    // Once a frame showed b.rs, Space marks it.
    assert!(!host.model().rows.rows.is_empty(), "b.rs shows");
    host.key(Action::ToggleApprove, false);
    host.settle();
    assert!(
        matches!(
            logged(&host.review).pop(),
            Some(review_core::store::events::Event::Mark(_))
        ),
        "the key marked nothing"
    );
}

/// After a format change, the review is verified only once the analysis showed, with the text
/// it brought and the reads it asked for, so the verification reads no text again.
#[test]
fn a_late_analysis_shows_before_the_review_is_verified() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), true);
    show_opened(&mut host);
    let analyzed = run_until(&mut host, |outcome| matches!(outcome, Outcome::Analyzed(_)));
    host.complete(analyzed);
    while let Some(job) = host.jobs.pop_front() {
        let completion = job.run();
        assert!(
            !matches!(*completion.outcome, Outcome::Verified(_)),
            "the review is verified before its analysis shows"
        );
        host.complete(completion);
    }
    host.draw();
    let verified = run_until(&mut host, |outcome| matches!(outcome, Outcome::Verified(_)));
    host.complete(verified);
}

/// A grant read again shows dimmed, at the same width, until the reading lands: it is the last
/// one read, not one known to apply now.
#[test]
fn a_grant_read_again_shows_as_the_last_one_read() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = checkout_host(dir.path());
    std::fs::write(repo.join("a.rs"), two_hunks("one", "2")).unwrap();
    host.update(Model::changed);
    host.settle();
    trust(&mut host);
    let label = |model: &Model| {
        crate::paint::bars::sync_label(
            model,
            &crate::paint::Palette::of(model),
            std::time::SystemTime::now(),
            f32::MAX,
            |text| scroll::float(text.chars().count()),
        )
        .into_iter()
        .find(|piece| piece.text == "trusted")
        .expect("the grant shows")
        .color
    };
    let palette = crate::paint::Palette::of(host.model());
    assert_eq!(label(host.model()), palette.flagged);

    // A capture of the same code reads trust again.
    host.update(Model::changed);
    let captured = host.run_newest_job();
    host.complete(captured);
    host.draw();
    assert!(host.model().trust_rereading());
    assert_eq!(label(host.model()), palette.faint);
    host.settle();
    assert_eq!(label(host.model()), palette.flagged);
}

/// The trust on the status line never goes unknown while it is read again: not as the pending
/// view gives way to the verified review, nor as a capture of the same code after it reads it.
#[test]
fn the_trust_shown_never_goes_unknown_across_promotion() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    // Every job lands with a frame after it, each of which shows the checkout untrusted.
    let land_all = |host: &mut Host| {
        while let Some(job) = host.jobs.pop_front() {
            let completion = job.run();
            host.complete(completion);
            host.draw();
            let bar = bar(host.model());
            assert!(bar.ends_with(" · untrusted"), "{bar}");
        }
    };
    land_all(&mut host);
    assert!(
        host.model().checking().is_none(),
        "the verified review shows"
    );
    host.update(Model::changed);
    land_all(&mut host);
}

/// A review key on the pending view records nothing, also once the verified review waits for a
/// frame, and is never replayed; once that frame shows the review, the key marks the unit
/// shown, naming its evidence there.
#[test]
fn a_key_on_the_pending_view_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    host.update(|model| model.show_file("b.rs"));
    host.dispatch(Action::FocusDiff);
    let events = std::fs::read(host.review.dir().events()).unwrap();

    let verified = run_until(&mut host, |outcome| matches!(outcome, Outcome::Verified(_)));
    host.complete(verified);
    host.key(Action::ToggleApprove, false);
    assert_eq!(host.model().status, super::pending::CHECKING);
    assert!(host.model().saves.is_empty() && host.model().pending.is_empty());
    host.settle();
    assert!(
        host.model().checking().is_none(),
        "the verified review shows"
    );
    assert_eq!(
        std::fs::read(host.review.dir().events()).unwrap(),
        events,
        "a key on the pending view recorded something"
    );
    // A repeat of the key held since the pending view acts on nothing the verified review
    // shows, even on the very unit its press was refused on.
    host.key(Action::ToggleApprove, true);
    host.settle();
    assert_eq!(
        std::fs::read(host.review.dir().events()).unwrap(),
        events,
        "a key held since the pending view marked the verified review"
    );

    let model = host.model();
    assert_eq!(model.selected.as_deref(), Some("b.rs"), "the file stays");
    let unit = model.hunk_units()[0];
    let evidence = model.view().unwrap().evidence_digest(unit).unwrap();
    host.key(Action::ToggleApprove, false);
    host.settle();
    let Some(review_core::store::events::Event::Mark(mark)) = logged(&host.review).pop() else {
        panic!("the key marked nothing");
    };
    assert_eq!((mark.revision, mark.unit), (unit.revision(), unit.index()));
    assert_eq!(mark.evidence, evidence);
}

/// The pending view opens the whole file, where the review keys mark nothing either, and the
/// overview, which reads the verified review, says it waits for it.
#[test]
fn the_pending_view_opens_the_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    host.update(|model| model.show_file("b.rs"));
    let read = run_until(&mut host, |outcome| {
        matches!(outcome, Outcome::TextsRead(_))
    });
    host.complete(read);
    let events = std::fs::read(host.review.dir().events()).unwrap();

    host.dispatch(Action::ToggleBuffer);
    assert_eq!(host.model().mode, Mode::Buffer);
    host.key(Action::ToggleApprove, false);
    assert_eq!(host.model().status, super::pending::CHECKING);
    host.dispatch(Action::ToggleOverview);
    assert_eq!(host.model().status, super::pending::WAITS);
    assert_eq!(host.model().overlay, None);
    assert!(host.model().checking().is_some(), "still pending");
    assert_eq!(std::fs::read(host.review.dir().events()).unwrap(), events);
}

/// The review verified behind its last recorded revision, after the format changed, derives the
/// standings, the partial approval and the note a review read from the start derives.
#[test]
fn the_verified_review_is_the_one_read_from_the_start() {
    for stale in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, _repo) = recorded_checkout(dir.path(), stale);
        show_opened(&mut host);
        host.settle();
        let model = host.model();
        assert!(model.checking().is_none(), "the verified review shows");
        let shown = &model.snapshot.as_ref().unwrap().history;
        let fresh = Review::load(&dir.path().join("data"), host.review.key())
            .unwrap()
            .history()
            .unwrap();
        assert_eq!(**shown, fresh, "stale: {stale}");
        let view = fresh.current_view().unwrap();
        assert_eq!(view.notes().len(), 1);
        assert!(matches!(
            view.since_reviewed(view.units_of_file("a.rs")[0]),
            Ok(Some(review_core::SinceReviewed::Delta(_)))
        ));
    }
}

/// A record another window appends while the review opens shows once the review is verified,
/// since the verification reads on from where the pending view read the log; a log replaced
/// meanwhile ends the review over the pending view.
#[test]
fn the_verification_reads_on_from_the_pending_view() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    let other = Review::load(&dir.path().join("data"), host.review.key()).unwrap();
    let history = other.history().unwrap();
    let view = history.current_view().unwrap();
    other
        .mark(&view, &view.units_of_file("b.rs"), MarkVerdict::Accepted)
        .unwrap();
    host.settle();
    assert_eq!(host.model().file_totals(), (1, 2), "b.rs is done");

    // A shorter log, which every platform tells apart from the one read, and where the
    // platform tells files apart, a copy of the same bytes.
    let shorter: fn(&[u8]) -> Vec<u8> = |bytes| {
        let last = bytes[..bytes.len() - 1]
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        bytes[..last].to_vec()
    };
    #[cfg(unix)]
    let replacements = [shorter, <[u8]>::to_vec];
    #[cfg(not(unix))]
    let replacements = [shorter];
    for replacement in replacements {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, _repo) = recorded_checkout(dir.path(), false);
        show_opened(&mut host);
        let log = host.review.dir().events();
        let copy = log.with_extension("copy");
        std::fs::write(&copy, replacement(&std::fs::read(&log).unwrap())).unwrap();
        std::fs::rename(&copy, &log).unwrap();
        host.settle();
        let model = host.model();
        assert_eq!(model.replaced(), Some(Replaced::Log));
        assert!(model.checking().is_some(), "the pending view stays");
        assert!(!model.busy());
        host.dispatch(Action::ToggleApprove);
        assert!(host.model().saves.is_empty());
    }
}

/// A whole file opened from the pending view opens again from the verified review, which shows
/// the file as the checkout has it now, or else the diff does: never the pending view's text
/// under the verified review's markers.
#[test]
fn a_whole_file_opened_while_pending_shows_the_verified_text() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    // The verification captures the checkout as it is now, past the recorded revision.
    std::fs::write(repo.join("b.rs"), "fn b() { b(); b(); }\n").unwrap();
    host.update(|model| model.show_file("b.rs"));
    let read = run_until(&mut host, |outcome| {
        matches!(outcome, Outcome::TextsRead(_))
    });
    host.complete(read);
    host.dispatch(Action::ToggleBuffer);
    assert_eq!(
        host.model().buffer.as_ref().unwrap().lines.lines,
        ["fn b() { b(); }"]
    );

    host.settle();
    let model = host.model();
    assert!(model.checking().is_none(), "the verified review shows");
    match &model.buffer {
        Some(buffer) => {
            assert_eq!(model.mode, Mode::Buffer);
            assert_eq!(buffer.lines.lines, ["fn b() { b(); b(); }"]);
        }
        None => assert_eq!(model.mode, Mode::Diff),
    }
}

/// A whole file reopened from the verified review shows its cursor where the reviewer left it
/// with the first verified frame, even a viewport away from the hunk the diff selects there.
#[test]
fn a_reopened_whole_file_shows_its_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    // The buffer shows ten rows, fewer than lie between a.rs's two changes, `let v10` and
    // `let v40`.
    let window = Geometry {
        heights: Heights {
            buffer: Some(10.0),
            ..fixed_window().heights
        },
        ..fixed_window()
    };
    let lay_out = |host: &mut Host| host.update(|model| model.layout(window, Instant::now()));
    lay_out(&mut host);
    show_opened(&mut host);
    host.update(|model| model.show_file("a.rs"));
    let read = run_until(&mut host, |outcome| {
        matches!(outcome, Outcome::TextsRead(_))
    });
    host.complete(read);
    host.dispatch(Action::FocusDiff);
    host.dispatch(Action::ToggleBuffer);
    lay_out(&mut host);
    // The cursor goes to `let v10`, whose change the verified review approved in part, so its
    // diff selects the change at `let v40` instead.
    let row = 11;
    host.update(|model| model.set_buffer_cursor(row, 0));
    lay_out(&mut host);

    host.settle();
    lay_out(&mut host);
    let model = host.model();
    assert!(model.checking().is_none(), "the verified review shows");
    assert_eq!(model.mode, Mode::Buffer);
    assert_eq!(model.buffer.as_ref().unwrap().cursor.row, row);
    let top = model.buffer_scroll.offset();
    assert!(
        top <= scroll::rows(row) && scroll::rows(row) < top + 10.0,
        "the cursor's row {row} shows below row {top}"
    );
}

/// A verified review with nothing to review, as after the checkout's changes were reverted,
/// takes the pending view's rows off the screen, so the empty review shows as one.
#[test]
fn a_verified_review_without_changes_clears_the_pending_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    host.dispatch(Action::FocusDiff);
    assert!(
        !host.model().rows.hunks.is_empty(),
        "the pending view shows the recorded changes"
    );
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(repo.join("b.rs"), "fn b() {}\n").unwrap();

    host.settle();
    let model = host.model();
    assert!(model.checking().is_none(), "the verified review shows");
    assert!(model.tree.is_empty(), "nothing is left to review");
    assert_eq!(model.selected, None);
    assert!(model.rows.hunks.is_empty() && model.rows.screen.is_empty());
    assert_eq!(model.mode, Mode::Tree);
}

/// While git is busy in the checkout, or cr's data is full, the review is verified on its last
/// recorded revision, which then takes marks.
#[test]
fn a_busy_or_full_checkout_verifies_its_last_recorded_revision() {
    for full in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mut host, repo) = recorded_checkout(dir.path(), false);
        // An edit the capture would show, were it taken.
        std::fs::write(repo.join("b.rs"), "fn b() { c(); }\n").unwrap();
        if full {
            let big = std::fs::File::create(dir.path().join("data/big")).unwrap();
            big.set_len(11 << 30).unwrap();
        } else {
            std::fs::write(repo.join(".git/MERGE_HEAD"), "").unwrap();
        }
        show_opened(&mut host);
        host.settle();
        let model = host.model();
        assert!(model.checking().is_none(), "full: {full}");
        assert_eq!(model.view().unwrap().number().get(), 2, "full: {full}");
        assert!(!model.view().unwrap().is_provisional());
        host.update(|model| model.show_file("b.rs"));
        host.dispatch(Action::FocusDiff);
        host.key(Action::ToggleApprove, false);
        host.settle();
        assert!(
            matches!(
                logged(&host.review).pop(),
                Some(review_core::store::events::Event::Mark(_))
            ),
            "full: {full}: {}",
            host.model().status
        );
    }
}

/// How many ticks wait that draw a frame for the count of revisions replayed.
fn checking_ticks(host: &Host) -> usize {
    host.timers
        .iter()
        .filter(|timer| matches!(*timer.outcome, Outcome::Tick(super::sync::Tick::Checking)))
        .count()
}

/// While the review state is checked, the bar counts the revisions the verification replayed
/// out of all the log recorded, at one width while it counts, the revision making way for it
/// when room runs out; a tick draws a frame for the count until the verification lands.
#[test]
fn the_bar_counts_the_revisions_the_verification_replayed() {
    use crate::paint::bars::checking_label;
    use review_core::Replay;
    let label = |replayed| {
        checking_label(Replay {
            replayed,
            total: 100,
        })
    };
    assert_eq!(label(7), "Checking review state   7/100");
    assert!(
        (0..=100).all(|replayed| label(replayed).chars().count() == label(0).chars().count()),
        "the count changes the label's width"
    );
    // The front ends keep thirty cells for the bar's left side, room for a review of up to
    // 999 revisions.
    let widest = checking_label(Replay {
        replayed: 0,
        total: 999,
    });
    assert!(widest.chars().count() <= 30, "{widest}");

    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    // In the room the front ends keep for it, the count shows alone.
    let narrow = |model: &Model| -> String {
        crate::paint::bars::sync_label(
            model,
            &crate::paint::Palette::of(model),
            std::time::SystemTime::now(),
            30.0,
            |text| scroll::float(text.chars().count()),
        )
        .into_iter()
        .map(|piece| piece.text)
        .collect()
    };
    assert_eq!(narrow(host.model()), "Checking review state 0/2");
    assert!(
        bar(host.model()).starts_with("rev 2 · "),
        "{}",
        bar(host.model())
    );
    assert_eq!(checking_ticks(&host), 1);

    // The verification replays both revisions before its outcome lands; meanwhile each tick
    // asks for the next.
    let verified = run_until(&mut host, |outcome| matches!(outcome, Outcome::Verified(_)));
    assert_eq!(narrow(host.model()), "Checking review state 2/2");
    let tick = |host: &mut Host| {
        let index = host
            .timers
            .iter()
            .position(|timer| matches!(*timer.outcome, Outcome::Tick(super::sync::Tick::Checking)))
            .expect("a tick waits");
        host.fire(index);
    };
    tick(&mut host);
    assert_eq!(checking_ticks(&host), 1, "the tick stopped while verifying");
    // Once the verified review waits for its frame, the ticks stop.
    host.complete(verified);
    tick(&mut host);
    assert_eq!(checking_ticks(&host), 0, "the tick runs on");
    host.draw();
    let bar = bar(host.model());
    assert!(!bar.contains("Checking"), "{bar}");
}

/// The kind of `outcome`, for what a test asserts of the jobs that brought it.
fn outcome_kind(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Opened(_) => "opened",
        Outcome::Analyzed(_) => "analyzed",
        Outcome::Verified(_) => "verified",
        Outcome::TextsRead(_) => "texts",
        Outcome::PdfRead(Some(_)) => "pdf",
        Outcome::PdfRead(None) => "pdf dropped",
        Outcome::Compared(Some(_)) => "compared",
        Outcome::Compared(None) => "compared dropped",
        _ => "other",
    }
}

/// Runs every job and timer as [`Host::settle`] does, and returns the lane each job ran in
/// with the kind of what it brought.
fn settle_lanes(host: &mut Host) -> Vec<(pool::Lane, &'static str)> {
    let mut lanes = Vec::new();
    loop {
        if let Some(job) = host.jobs.pop_front() {
            let lane = job.lane();
            let completion = job.run();
            lanes.push((lane, outcome_kind(&completion.outcome)));
            host.complete(completion);
        } else if !host.timers.is_empty() {
            host.fire(0);
        } else if host.model().publishes() {
            host.draw();
        } else {
            return lanes;
        }
    }
}

/// The review's verification waits in the pool's background lane, behind what the reviewer
/// looks at: the opened review, the analysis of the revision it shows and its files' texts,
/// so a replay of the whole history never holds them back.
#[test]
fn the_verification_runs_behind_what_the_reviewer_looks_at() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), true);
    let lanes = settle_lanes(&mut host);
    let lane_of = |kind: &str| {
        lanes
            .iter()
            .find(|(_, ran)| *ran == kind)
            .map(|(lane, _)| *lane)
    };
    assert_eq!(lane_of("opened"), Some(pool::Lane::Selected));
    assert_eq!(lane_of("analyzed"), Some(pool::Lane::Selected));
    let texts: Vec<pool::Lane> = lanes
        .iter()
        .filter(|(_, ran)| *ran == "texts")
        .map(|(lane, _)| *lane)
        .collect();
    assert!(
        texts.contains(&pool::Lane::Shown),
        "the other files' texts are read"
    );
    assert!(!texts.contains(&pool::Lane::Background));
    assert_eq!(lane_of("verified"), Some(pool::Lane::Background));
    assert!(
        host.model().checking().is_none(),
        "the verified review shows"
    );
}

/// A PDF read that waits for a worker while the reviewer moves on reads nothing and lands
/// nothing, even once its file shows again, and the file shown then is read next; a read that
/// ran to its end before the reviewer moved on lands under no other file.
#[cfg(feature = "pdf")]
#[test]
fn a_superseded_preview_reads_nothing_and_lands_nothing() {
    use crate::pdf::testing::pdf;
    let dir = tempfile::tempdir().unwrap();
    let text = |text: &str| text.as_bytes().to_vec();
    let review = review_of(
        dir.path(),
        &[
            ("a.rs", text("fn a() {}\n")),
            ("b.pdf", pdf(&["One"])),
            ("c.pdf", pdf(&["Two", "Three"])),
        ],
        &[
            ("a.rs", text("fn a() { a(); }\n")),
            ("b.pdf", pdf(&["1"])),
            ("c.pdf", pdf(&["2", "3"])),
        ],
    );
    let mut host = Host::open(review);
    let navigator = |host: &Host| host.model().page_navigator().unwrap_or_default();
    // Away to c.pdf and back before the read of b.pdf runs: the one read under way is
    // superseded and lands nothing, c.pdf is never read, and b.pdf is read again.
    host.update(|model| model.show_file("b.pdf"));
    host.update(|model| model.show_file("c.pdf"));
    host.update(|model| model.show_file("b.pdf"));
    let reads: Vec<_> = settle_lanes(&mut host)
        .into_iter()
        .filter(|(_, kind)| kind.starts_with("pdf"))
        .collect();
    assert_eq!(
        reads,
        [
            (pool::Lane::Selected, "pdf dropped"),
            (pool::Lane::Selected, "pdf")
        ]
    );
    assert!(navigator(&host).contains("1/1"), "{}", navigator(&host));

    // A read of c.pdf that ends after the reviewer went back to b.pdf shows nothing, and c.pdf
    // is read again once it shows.
    host.update(|model| model.show_file("c.pdf"));
    let late = run_until(&mut host, |outcome| {
        matches!(outcome, Outcome::PdfRead(Some(_)))
    });
    host.update(|model| model.show_file("b.pdf"));
    host.complete(late);
    assert!(navigator(&host).contains("1/1"), "{}", navigator(&host));
    host.update(|model| model.show_file("c.pdf"));
    assert!(navigator(&host).is_empty(), "{}", navigator(&host));
    host.settle();
    assert!(navigator(&host).contains("1/2"), "{}", navigator(&host));
}

/// Stepping through a PDF's pages faster than they render renders only the page pair that
/// shows: a render of a page pair stepped past while it waited renders nothing.
#[cfg(feature = "pdf")]
#[test]
fn a_page_stepped_past_renders_nothing() {
    use crate::pdf::testing::pdf;
    let dir = tempfile::tempdir().unwrap();
    let review = review_of(
        dir.path(),
        &[("a.pdf", pdf(&["One", "Two", "Three"]))],
        &[("a.pdf", pdf(&["1", "2", "3"]))],
    );
    let mut host = Host::open(review);
    host.update(|model| model.show_file("a.pdf"));
    host.settle();
    assert_eq!(compared_page(&host), Some((Some(1), Some(1))));
    host.dispatch(Action::NextPage);
    host.dispatch(Action::NextPage);
    let renders: Vec<_> = settle_lanes(&mut host)
        .into_iter()
        .filter(|(_, kind)| kind.starts_with("compared"))
        .collect();
    assert_eq!(
        renders,
        [
            (pool::Lane::Selected, "compared dropped"),
            (pool::Lane::Selected, "compared")
        ]
    );
    assert_eq!(compared_page(&host), Some((Some(3), Some(3))));
}

/// The file header's right side keeps its column as the verified review takes the pending
/// view's place: while the review state is checked, the meter's place is blank but as wide as
/// the meter that then shows there, so the file's length and its line counts never move.
#[test]
fn the_file_header_keeps_its_columns_across_promotion() {
    use crate::paint::Room;
    use crate::paint::header::{FileHeader, file_header};
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = recorded_checkout(dir.path(), false);
    show_opened(&mut host);
    let header = |model: &Model| {
        let room = Room {
            budget: 160.0,
            cell: 1.0,
            gap: 2.0,
        };
        file_header(model, &crate::paint::Palette::of(model), room, 1.0).expect("a file shows")
    };
    // Where each part of the right side starts, counted from the right edge, as the front ends
    // lay them out: the file's length, the line counts, then the meter's bar and its counts.
    let columns = |header: &FileHeader| {
        let width = |text: &str| text.chars().count();
        let meter = header.meter.as_ref().expect("the meter has its place");
        let meter_width = 5 + 1 + width(&meter.counts.text);
        let counts = header.counts.as_ref().expect("the line counts show");
        let counts_width = width(&counts[0].text) + 2 + width(&counts[1].text);
        let lines = header.lines.as_ref().expect("the file's length shows");
        [
            meter_width,
            meter_width + 2 + counts_width,
            meter_width + 2 + counts_width + 2 + width(&lines.text),
        ]
    };
    let pending = header(host.model());
    assert!(host.model().checking().is_some(), "the pending view shows");
    // Nothing counts yet: the meter's place shows nothing.
    let blank = pending.meter.as_ref().unwrap();
    assert!(
        blank.counts.text.trim().is_empty(),
        "{:?}",
        blank.counts.text
    );
    assert!(blank.bar.track.a <= 0.0 && blank.bar.fill.a <= 0.0);
    host.settle();
    assert!(
        host.model().checking().is_none(),
        "the verified review shows"
    );
    let verified = header(host.model());
    let counted = &verified.meter.as_ref().unwrap().counts.text;
    // The meter counts in the place kept for it: a.rs's one unit, changed since it was marked.
    assert_eq!(counted.trim(), "0/1");
    assert_eq!(columns(&pending), columns(&verified));
}

/// Starts each of `jobs` on `pool`, as a front end starts them in the app's pool, and returns
/// where their completions arrive, in the order they end.
fn start_on(pool: &pool::Pool, jobs: Vec<Job>) -> std::sync::mpsc::Receiver<Completion> {
    let (sender, landed) = std::sync::mpsc::channel();
    for job in jobs {
        let sender = sender.clone();
        job.start_on(pool, move |completion| {
            sender.send(completion).ok();
        });
    }
    landed
}

/// The watch on the grant runs apart from the pool: with both workers busy, a lookup of a pull
/// request holding the one background work may take, it still reads the grant, so a revocation
/// in another window stops this window's servers on time.
#[test]
fn the_guard_on_the_grant_never_waits_for_a_worker() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = checkout_host(dir.path());
    let pool = pool::Pool::new(2);
    let lookup = pool.hold(pool::Lane::Background);
    let preview = pool.hold(pool::Lane::Selected);
    host.update(Model::guard_due);
    let guard = host.jobs.pop_back().expect("the guard is queued");
    let landed = start_on(&pool, vec![guard]);
    let completion = landed
        .recv_timeout(pool::PATIENCE)
        .expect("the guard waited for a worker");
    assert!(matches!(*completion.outcome, Outcome::Guarded(_)));
    drop((lookup, preview));
}

/// A checkout whose one recorded revision changes `files` files and adds `added`, opened as
/// the model's opener opens it, its last recorded revision showing while it is verified.
fn checkout_of_files(dir: &Path, files: usize, added: &[(&str, &[u8])]) -> Host {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "--quiet", "--initial-branch=main"]);
    // Each file's own text, so each version is a blob of its own.
    let write = |label: &str| {
        for file in 0..files {
            let text = statements(&[10], &format!("{label}{file}"));
            std::fs::write(repo.join(format!("f{file:02}.rs")), text).unwrap();
        }
    };
    write("0");
    git_in(&repo, &["add", "--all"]);
    git_in(&repo, &["commit", "--quiet", "-m", "base"]);
    git_in(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let data = dir.join("data");
    let review = Review::open_checkout(&data, review_core::Repo::discover(&repo).unwrap()).unwrap();
    write("1");
    for (path, bytes) in added {
        std::fs::write(repo.join(path), bytes).unwrap();
    }
    review.refresh().unwrap();
    drop(review);
    let loaded =
        crate::launch::open_checkout(&data, review_core::Repo::discover(&repo).unwrap()).unwrap();
    Host::opening(loaded, settings())
}

/// The head blob of the file at `path` in what `model`'s diff lays out.
fn head_of(model: &Model, path: &str) -> ObjectId {
    let laid = model.laid_out().unwrap();
    let view = laid.source().unwrap();
    let unit = view.units_of_file(path)[0];
    view.file(unit).unwrap().new.as_ref().unwrap().oid.clone()
}

/// The review of 20 changed files opened, its pending view shown, and the jobs it queued but
/// the verification: the first of them reads the first file's text with its analysis, and a
/// read of the next 16 files' texts waits.
fn opened_with_texts_queued(dir: &Path) -> (Host, Vec<Job>) {
    let mut host = checkout_of_files(dir, 20, &[]);
    show_opened(&mut host);
    let queued = host
        .jobs
        .drain(..)
        .filter(|job| job.lane() != pool::Lane::Background)
        .collect();
    (host, queued)
}

/// A file the reviewer selects that a queued read of other files' texts has claimed shows
/// with its text before that read runs: its own read goes ahead of it, whatever it claimed.
#[test]
fn a_file_a_queued_read_claimed_shows_without_waiting_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, queued) = opened_with_texts_queued(dir.path());
    let head = head_of(host.model(), "f05.rs");
    assert!(
        host.model().requested.claims().any(|claim| *claim == head),
        "the queued read has it"
    );
    host.update(|model| model.show_file("f05.rs"));
    // Held jobs stand in for the verification and for a read that runs.
    let pool = pool::Pool::new(2);
    let verification = pool.hold(pool::Lane::Background);
    let running = pool.hold(pool::Lane::Shown);
    let landed = start_on(
        &pool,
        queued.into_iter().chain(host.jobs.drain(..)).collect(),
    );
    drop(running);
    loop {
        let completion = landed.recv_timeout(pool::PATIENCE).unwrap();
        let read = match &*completion.outcome {
            Outcome::TextsRead(read) => Some(read.urgent),
            _ => None,
        };
        assert_ne!(read, Some(false), "the queued read ran first");
        host.complete(completion);
        if read == Some(true) {
            break;
        }
    }
    host.draw();
    assert!(!host.model().rows.rows.is_empty(), "the file shows");
    assert!(host.model().blobs.texts.contains_key(&head));
    assert!(
        host.model().requested.claims().any(|claim| *claim == head),
        "the queued read has not run"
    );
    drop(verification);
}

/// Moving across files whose texts are not read while every worker is busy leaves one read
/// of the selected file's text outstanding, which reads the file the reviewer stopped at
/// once a worker frees up, and only that one.
#[test]
fn moving_across_unread_files_reads_only_the_last() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, queued) = opened_with_texts_queued(dir.path());
    let pool = pool::Pool::new(2);
    let verification = pool.hold(pool::Lane::Background);
    let running = pool.hold(pool::Lane::Shown);
    for path in ["f17.rs", "f18.rs", "f19.rs"] {
        host.update(|model| model.show_file(path));
    }
    let last = head_of(host.model(), "f19.rs");
    let jobs: Vec<Job> = queued.into_iter().chain(host.jobs.drain(..)).collect();
    let count = jobs.len();
    let landed = start_on(&pool, jobs);
    drop(running);
    let urgent: Vec<Vec<ObjectId>> = (0..count)
        .filter_map(
            |_| match *landed.recv_timeout(pool::PATIENCE).unwrap().outcome {
                Outcome::TextsRead(read) if read.urgent => Some(read.asked),
                _ => None,
            },
        )
        .collect();
    assert_eq!(urgent, [vec![last]]);
    drop(verification);
}

/// The selected file's read of an oversized blob, such as a large asset's, looks its size up
/// and reads none of its bytes, so a move to a file whose text is not read while it runs has
/// that file read as soon as the lookup ends.
#[test]
fn a_move_past_an_oversized_file_reads_the_next_file_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let ceiling = usize::try_from(review_core::git::MAX_TEXT_BLOB_BYTES).unwrap();
    let asset = vec![b'a'; ceiling + 1];
    let mut host = checkout_of_files(dir.path(), 20, &[("asset.txt", &asset)]);
    show_opened(&mut host);
    // A held job stands in for the verification, which keeps one worker busy.
    let pool = pool::Pool::new(2);
    let verification = pool.hold(pool::Lane::Background);
    let urgent = |landed: &std::sync::mpsc::Receiver<Completion>| loop {
        let completion = landed.recv_timeout(pool::PATIENCE).unwrap();
        if matches!(&*completion.outcome, Outcome::TextsRead(read) if read.urgent) {
            return completion;
        }
    };

    host.update(|model| model.show_file("asset.txt"));
    let landed = start_on(&pool, host.jobs.drain(..).collect());
    let asset_read = urgent(&landed);
    let Outcome::TextsRead(read) = &*asset_read.outcome else {
        panic!("not a text read");
    };
    let head = head_of(host.model(), "asset.txt");
    assert_eq!(
        (read.asked.as_slice(), read.texts.len()),
        ([head.clone()].as_slice(), 0)
    );

    // The reviewer moves on while, as the model knows, that read is outstanding.
    host.update(|model| model.show_file("f19.rs"));
    host.complete(asset_read);
    assert!(host.model().blobs.textless.contains(&head));
    let landed = start_on(&pool, host.jobs.drain(..).collect());
    let next = urgent(&landed);
    host.complete(next);
    host.draw();
    assert!(
        host.model()
            .blobs
            .texts
            .contains_key(&head_of(host.model(), "f19.rs"))
    );
    assert!(!host.model().rows.rows.is_empty(), "the next file shows");
    drop(verification);
}

/// A file search a newer query superseded while it waited for a worker lists and matches
/// nothing; only the newest search lists the whole tree and finds the file.
#[test]
fn a_superseded_file_search_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.dispatch(Action::FindFiles);
    host.settle();
    host.dispatch(Action::PickerScope);
    for query in ["e", "en", "engine"] {
        host.update(|model| model.edit_field(Field::PickerQuery, query));
    }
    let newest = host.model().picker_generation().unwrap();
    let pool = pool::Pool::new(2);
    let first = pool.hold(pool::Lane::Selected);
    let second = pool.hold(pool::Lane::Selected);
    let jobs: Vec<Job> = host.jobs.drain(..).collect();
    let count = jobs.len();
    assert_eq!(count, 4, "the scope's search and one per query wait");
    let landed = start_on(&pool, jobs);
    drop(first);
    for _ in 0..count {
        let completion = landed.recv_timeout(pool::PATIENCE).unwrap();
        let Subject::Search(generation) = completion.subject else {
            panic!("not a search");
        };
        let Outcome::Found(found) = *completion.outcome else {
            panic!("not what a search finds");
        };
        if generation == newest {
            assert!(found.listing.is_some(), "the newest search lists the tree");
            assert_eq!(found.entries[0].path, "src/engine.rs");
        } else {
            assert!(found.listing.is_none() && found.entries.is_empty());
        }
    }
    drop(second);
}
