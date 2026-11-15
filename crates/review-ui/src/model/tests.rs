//! The model on a fake synchronous host, for the races between input and background work that
//! scripted QA cannot pin: jobs run inline only when a test runs them, in the order it picks,
//! and timers fire only by hand.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use indoc::{formatdoc, indoc};
use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};
use review_core::tree::NodeKind;
use review_core::{MarkVerdict, Review};

use super::buffer::{Definition, Lookup};
use super::effects::{Completion, Job, Outcome, Subject};
use super::picker::{Found, PreviewText, Unchanged};
use super::*;
use crate::frame::{Chrome, Geometry, Heights, List, Scale, Sizes};
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
    jobs: VecDeque<Job>,
    timers: Vec<Completion>,
    /// The minute clock's ticks, and a live review's periodic captures, which keep themselves
    /// going, so only a test fires them.
    clocks: Vec<Completion>,
    /// Jobs that wait for something outside the app, which only a test runs.
    waits: Vec<Job>,
    copied: Vec<String>,
    /// How the model asked the front end to scroll its sheets.
    sheet_steps: Vec<SheetStep>,
    /// How many times the model asked the front end to quit.
    quits: usize,
    /// The diagnostics the model logged.
    logged: Vec<String>,
}

impl Host {
    /// The window over `review`, opened and settled: the review read, its texts and highlights
    /// in, and every save done.
    fn open(review: Review) -> Self {
        Self::open_with(review, settings())
    }

    /// The window over `review` with `settings`, opened and settled.
    fn open_with(review: Review, settings: Settings) -> Self {
        let history = review.history().unwrap();
        let mut host = Self {
            driver: Driver::new(Model::new(settings, Scale::fixed(cells()), fixed_window())),
            jobs: VecDeque::new(),
            timers: Vec::new(),
            clocks: Vec::new(),
            waits: Vec::new(),
            copied: Vec::new(),
            sheet_steps: Vec::new(),
            quits: 0,
            logged: Vec::new(),
        };
        let opener: Opener = Box::new(move || {
            Ok(Loaded {
                review,
                history,
                found: None,
            })
        });
        host.update(|model| model.open(opener));
        host.settle();
        assert!(
            matches!(host.model().phase, Phase::Ready),
            "the review opens"
        );
        host
    }

    fn model(&self) -> &Model {
        self.driver.model()
    }

    /// Changes the model as a front end does, keeping the effects for the test to run.
    fn update<R>(&mut self, change: impl FnOnce(&mut Model) -> R) -> R {
        let Updated { value, effects } = self.driver.update(change);
        for effect in effects {
            match effect {
                Effect::Spawn(job) => self.jobs.push_back(job),
                Effect::Wait(job) => self.waits.push(job),
                // The minute clock and the periodic capture keep themselves going, so they
                // wait apart from the timers that settle.
                Effect::After(_, completion)
                    if matches!(
                        *completion.outcome,
                        Outcome::Tick(super::sync::Tick::Minute | super::sync::Tick::Reconcile)
                    ) =>
                {
                    self.clocks.push(completion);
                }
                Effect::After(_, completion) => self.timers.push(completion),
                Effect::Copy(text) | Effect::CopyPrimary(text) => self.copied.push(text),
                Effect::ScrollSheet(step) => self.sheet_steps.push(step),
                Effect::Quit => self.quits += 1,
                Effect::Title(_) | Effect::Fullscreen => {}
                Effect::Log(message) => self.logged.push(message),
            }
        }
        value
    }

    fn dispatch(&mut self, action: Action) {
        self.update(|model| model.dispatch(action));
    }

    /// Runs the oldest waiting job and returns its completion, not yet applied.
    fn run_job(&mut self) -> Completion {
        let job = self.jobs.pop_front().expect("a job waits");
        job()
    }

    /// Runs the newest waiting job and returns its completion, not yet applied.
    fn run_newest_job(&mut self) -> Completion {
        let job = self.jobs.pop_back().expect("a job waits");
        job()
    }

    /// Hands a job's completion to the model on the UI thread, as the front end does when the
    /// job ends.
    fn complete(&mut self, completion: Completion) {
        self.update(|model| model.complete(completion));
    }

    /// Fires the `index`th waiting timer.
    fn fire(&mut self, index: usize) {
        let completion = self.timers.remove(index);
        self.complete(completion);
    }

    /// Runs every job and fires every timer, and what they start in turn, until none waits.
    fn settle(&mut self) {
        loop {
            if let Some(job) = self.jobs.pop_front() {
                let completion = job();
                self.complete(completion);
            } else if !self.timers.is_empty() {
                self.fire(0);
            } else {
                break;
            }
        }
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
                token_emphasis: config.token_emphasis,
            },
            preview: None,
        },
        themes,
        styles,
        layout: PaneLayout {
            sidebar: config.layout.sidebar,
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
    host.dispatch(Action::NextHunk);
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
        let hunks =
            rows::present(rows::build(&view, &units, &folds), Presentation::default()).hunks;
        assert!(hunks.len() > 2, "the engine has several hunks");
        review
            .mark(&view, &hunks[0], MarkVerdict::Accepted)
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
    assert!(!host.model().marking);
    let model = host.model();
    assert_eq!(model.rows.files[0].path, engine);
    assert_eq!(model.hunk, opened_at);
    let view = model.view().unwrap();
    let approved = |hunk: usize| {
        crate::state::badges(&view, &model.rows.hunks[hunk], &model.pending)
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
/// trusted review.
#[test]
fn go_to_definition_is_off_without_language_servers() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open_with(
        demo(dir.path()),
        Settings {
            language_servers: false,
            ..settings()
        },
    );
    assert!(host.model().trusted, "the demo's head is a local branch");
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    let model = host.model();
    assert!(!model.looking_up);
    assert!(host.jobs.is_empty(), "no lookup runs");
    assert!(model.servers.lock().is_empty());
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
    let mut host = Host::open(demo(dir.path()));
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    let asked = lookup(host.model());
    host.dispatch(Action::ToggleTrust);
    assert!(!host.model().trusted);
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

/// Opens the selected file in the buffer with the cursor on a name, ready for a lookup.
fn buffer_on_a_name(host: &mut Host) {
    host.update(|model| model.show_file("src/engine.rs"));
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

/// A definition that arrives after trust was revoked is dropped, and the servers the lookup
/// held stop once it ends.
#[cfg(target_os = "linux")]
#[test]
fn a_definition_after_trust_is_revoked_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    assert!(host.model().trusted, "the demo's head is a local branch");
    buffer_on_a_name(&mut host);
    host.dispatch(Action::GoToDefinition);
    assert!(host.model().looking_up);
    assert_eq!(host.jobs.len(), 1, "the lookup runs in the background");
    let asked = lookup(host.model());
    let buffer = host.model().buffer.as_ref().unwrap().id;

    // A language server runs, which the lookup holds while it waits for the answer; `cat`
    // stands in for it, a process that runs until it is stopped.
    let server = crate::lsp::LanguageServer::stand_in("cat", dir.path()).unwrap();
    let process = server.process_id();
    let servers = Arc::clone(&host.model().servers);
    let mut held = servers.lock();
    held.insert(("rust", dir.path().to_owned()), server);

    // Trust is revoked while the language server answers, so the servers cannot stop yet.
    host.dispatch(Action::ToggleTrust);
    assert!(!host.model().trusted);
    drop(held);
    assert_eq!(servers.lock().len(), 1, "the lookup still held the server");
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
    // The answer stopped the server: it is gone from the map and its process from the system.
    assert!(model.servers.lock().is_empty());
    assert!(
        !Path::new(&format!("/proc/{process}")).exists(),
        "the server's process {process} still runs"
    );
}

/// A definition lands only in the buffer and revision it was asked for: an answer for a buffer
/// since replaced, or for another revision, is dropped, and one for what shows moves there.
#[test]
fn a_definition_lands_only_where_it_was_asked() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
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
    assert_eq!(host.model().marks.len(), 2, "one mark saves, two wait");
    let wanted = host.model().pending.clone();
    assert!(
        wanted
            .values()
            .any(|&verdict| verdict == MarkVerdict::Unseen)
    );

    host.dispatch(Action::Quit);
    assert_eq!(host.quits, 0, "nothing quits while marks wait");
    assert_eq!(host.model().status, "Saving 3 marks before quitting…");

    // Each saved mark starts the next; the last one quits, once.
    let saved = host.run_job();
    host.complete(saved);
    assert_eq!(host.quits, 0);
    assert_eq!(host.model().status, "Saving 2 marks before quitting…");
    host.settle();
    assert_eq!(host.quits, 1);
    assert_eq!(host.model().quitting, Quitting::Done);

    // The review recorded what the reviewer saw: the last verdict of each unit.
    assert_eq!(recorded(host.model(), &wanted), wanted);
}

/// The verdicts the review recorded for the units of `wanted`, as the marks name them: an
/// approval, or a reset for a unit nobody reviewed.
fn recorded(model: &Model, wanted: &HashMap<UnitId, MarkVerdict>) -> HashMap<UnitId, MarkVerdict> {
    let review = model.review.clone().unwrap();
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
        .concat()
        .into_iter()
        .map(|unit| (unit, MarkVerdict::Unseen))
        .collect();
    let before = recorded(host.model(), &units);
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
        assert!(host.model().marks.is_empty(), "{action:?} queued nothing");
        assert_eq!(host.model().pending, wanted, "{action:?} changed nothing");
        assert_eq!(
            host.model().status,
            "Not marked while quitting. Saving 1 mark before quitting…"
        );
    }

    // Only the mark made before quitting is recorded.
    host.settle();
    assert_eq!(host.quits, 1);
    let mut expected = before;
    expected.extend(wanted);
    assert_eq!(recorded(host.model(), &units), expected);
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
    assert_eq!(host.model().marks.len(), 2, "one mark saves, two wait");
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
    assert_eq!(model.marks.len(), 2, "both approvals still wait");
    assert_eq!(
        model.pending, wanted,
        "every approval still shows as pending"
    );
    assert_eq!(model.status, "Saving 3 marks before quitting…");
    assert_eq!(host.quits, 0, "the second quit leaves no sooner");

    host.settle();
    assert_eq!(host.quits, 1);
    assert_eq!(recorded(host.model(), &wanted), wanted);
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
    host.model()
        .review
        .clone()
        .unwrap()
        .dir()
        .root()
        .to_path_buf()
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
    assert_eq!(recorded(host.model(), &wanted), wanted);
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
    assert_eq!(host.quits, 1);

    // The second quit while an approval saves and another waits, three hunks approved
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/parser.rs"));
    host.dispatch(Action::FocusDiff);
    for _ in 0..3 {
        host.dispatch(Action::ToggleApprove);
    }
    assert_eq!(host.model().marks.len(), 2);
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
        host.model().marks.len(),
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
    let review = host.model().review.clone().unwrap();
    assert_eq!(crate::folders::load(&review), Ok(Vec::<String>::new()));
}

/// A copy hands the selected text to the clipboard, and a drag that selects text hands it to
/// the primary selection when the button comes up; a click that selects nothing copies nothing.
#[test]
fn selections_reach_the_clipboards() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    host.update(|model| model.show_file("src/engine.rs"));
    host.dispatch(Action::FocusDiff);
    let (row, line) = {
        let model = host.model();
        let row = (0..model.rows.rows.len())
            .find(|&row| {
                model.rows.rows[row].is_code()
                    && model
                        .copyable(Pane::Diff, Column::Whole, row)
                        .is_some_and(|code| code.len() > 4)
            })
            .unwrap();
        let line = model
            .copyable(Pane::Diff, Column::Whole, row)
            .unwrap()
            .to_owned();
        (row, line)
    };
    let press = |col| TextPress {
        pane: Pane::Diff,
        column: Column::Whole,
        pos: TextPos { row, col },
        clicks: 1,
        extend: false,
    };

    // A drag over the first four bytes of the line.
    host.update(|model| model.press_text(press(0)));
    host.update(|model| model.drag_text(Pane::Diff, TextPos { row, col: 4 }));
    host.update(Model::release_text);
    assert_eq!(host.copied, [line[..4].to_owned()]);

    // Copying takes the same text.
    host.dispatch(Action::Copy);
    assert_eq!(host.copied.len(), 2);
    assert_eq!(host.copied[1], line[..4]);
    assert_eq!(host.model().status, "Copied 1 line.");

    // A click without a drag leaves an empty selection, which reaches neither clipboard.
    host.update(|model| model.press_text(press(2)));
    host.update(Model::release_text);
    host.dispatch(Action::Copy);
    assert_eq!(host.copied.len(), 2);
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
    let units = model.rows.hunks.get(hunk).cloned().unwrap_or_default();
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
    host.update(|model| model.show_file("Makefile"));
    host.dispatch(Action::NextFile);
    assert_eq!(shown(host.model()), "Makefile");
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
        model
            .hunk_units()
            .iter()
            .all(|&unit| model.folds.is_folded(&view, unit))
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
fn review_of(dir: &Path, base: &[(&str, &str)], head: &[(&str, &str)]) -> Review {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            // Isolated from the user's configuration, with a fixed identity.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
    };
    let write = |files: &[(&str, &str)]| {
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
    let analysis = view.analysis().unwrap();
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

/// A host over `review` whose highlighting has `workers` workers, opened up to the jobs its
/// opening starts, none of which has run.
fn open_unsettled(review: Review, workers: usize) -> Host {
    let history = review.history().unwrap();
    let mut host = Host {
        driver: Driver::new(Model::new(
            settings(),
            Scale::fixed(cells()),
            fixed_window(),
        )),
        jobs: VecDeque::new(),
        timers: Vec::new(),
        clocks: Vec::new(),
        waits: Vec::new(),
        copied: Vec::new(),
        sheet_steps: Vec::new(),
        quits: 0,
        logged: Vec::new(),
    };
    let opener: Opener = Box::new(move || {
        Ok(Loaded {
            review,
            history,
            found: None,
        })
    });
    host.update(|model| model.highlights.workers = workers);
    host.update(|model| model.open(opener));
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
        .unwrap()
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
    let landed = held();
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
    let landed = stale();
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
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
    let (review, repo) = worktree_review_of(
        dir,
        &[("a.rs", &two_hunks("1", "2"))],
        &[("a.rs", &two_hunks("one", "two"))],
    );
    let mut host = Host::open(review);
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
    assert!(model.status.starts_with("The working tree changed."));
}

/// A review key on a hunk that just changed under it is refused, and accepted once the change
/// has been on screen for a moment; marking records the capture it was made on.
#[test]
fn a_review_key_on_a_hunk_that_just_changed_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    edit_and_capture(&mut host, &repo, "uno", "two");
    assert!(selected_text(host.model()).contains("uno"));

    host.dispatch(Action::ToggleApprove);
    assert!(host.model().status.starts_with("Not marked"));
    assert!(host.model().pending.is_empty() && host.model().marks.is_empty());

    host.update(|model| model.live.as_mut().unwrap().age(Duration::from_secs(2)));
    host.dispatch(Action::ToggleApprove);
    assert!(!host.model().pending.is_empty(), "the mark is queued");
    host.settle();
    let review = host.model().review.clone().unwrap();
    let recorded = review.history().unwrap();
    assert_eq!(recorded.revisions().len(), 2, "the capture was recorded");
    assert!(!host.model().view().unwrap().is_provisional());
}

/// A key held down since before a hunk changed never marks it, however long it repeats; a new
/// press does.
#[test]
fn a_key_held_across_a_change_never_marks_it() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
    edit_and_capture(&mut host, &repo, "uno", "two");
    host.update(|model| {
        let live = model.live.as_mut().unwrap();
        live.age(Duration::from_secs(3));
        live.hold_since(Duration::from_secs(5));
    });
    host.dispatch(Action::ToggleApprove);
    assert!(host.model().status.starts_with("Not marked"), "still held");
    assert!(host.model().marks.is_empty());

    host.update(|model| model.live.as_mut().unwrap().age(Duration::ZERO));
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
    assert!(!host.model().marks.is_empty() || host.model().marking);

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
    // ...which the first replaces once help closes.
    host.dispatch(Action::CloseOverlay);
    assert_eq!(host.model().shown, shown + 1);
    assert!(selected_text(host.model()).contains("uno"));
    host.update(|model| model.live.as_mut().unwrap().age(Duration::from_secs(2)));

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
    let wait = host.waits.pop().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || sender.send(wait()).ok());
    let waited = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the watch sees the change");
    host.complete(waited);
    assert_eq!(host.waits.len(), 1, "the next wait starts");
    host.settle();
    assert!(selected_text(host.model()).contains("uno"));
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

/// A capture cuts the way back from a definition into a buffer of the old snapshot, whose
/// change markers number the old snapshot's hunks.
#[test]
fn a_capture_cuts_the_way_back_to_an_old_review_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, repo) = live_host(dir.path());
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

/// A folder the tree cursor lands on because the one under it went is new under the keys, so
/// a review key on it is refused at first, however unchanged its files are.
#[test]
fn a_folder_the_cursor_lands_on_refuses_review_keys_at_first() {
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
        host.model().status.starts_with("Not marked"),
        "{}",
        host.model().status
    );
    assert!(host.model().marks.is_empty());
}

/// A capture of the snapshot on screen with marks another window saved since shows them, and
/// so does `R`, which reads them first.
#[test]
fn marks_saved_elsewhere_show_with_the_next_capture() {
    let dir = tempfile::tempdir().unwrap();
    let (mut host, _repo) = live_host(dir.path());
    assert_eq!(host.model().file_totals(), (0, 1));
    let review = host.model().review.clone().unwrap();
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
