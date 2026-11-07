//! The model on a fake synchronous host, for the races between input and background work that
//! scripted QA cannot pin: jobs run inline only when a test runs them, in the order it picks,
//! and timers fire only by hand.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

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
    /// The minute clock's ticks, which only a test fires.
    clocks: Vec<Completion>,
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
                // The minute clock keeps itself going, so it waits apart from the timers that
                // settle.
                Effect::After(_, completion)
                    if matches!(
                        *completion.outcome,
                        Outcome::Tick(super::sync::Tick::Minute)
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

/// Moves the tree cursor down as a held key does, faster than the cursor's rest.
fn hold_down(host: &mut Host) {
    host.update(|model| {
        model.last_move = Some(Instant::now());
        model.dispatch(Action::TreeDown);
    });
}

/// A held key previews each file by its header; the rest after an earlier move lands after a
/// newer move and builds nothing, and only the newest move's rest builds the diff, of the file
/// under the cursor.
#[test]
fn a_stale_preview_rest_builds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let [start, first, second] = three_files(host.model());
    let start_path = path_at(host.model(), start);
    host.update(|model| model.show_file(&start_path));

    // Two moves of a held key: each shows its file's header alone and waits for a rest.
    hold_down(&mut host);
    hold_down(&mut host);
    assert_eq!(host.timers.len(), 2);
    let newest = host.model().preview;
    assert!(newest.is_some());
    let second_path = path_at(host.model(), second);
    assert_ne!(path_at(host.model(), first), second_path);
    assert_eq!(host.model().selected.as_deref(), Some(second_path.as_str()));

    // The first move's rest comes late: the second file still shows its header alone.
    host.fire(0);
    assert_eq!(host.model().preview, newest);
    assert!(host.model().rows.rows.is_empty(), "no diff is built early");
    assert_eq!(host.model().rows.files[0].path, second_path);

    // The second move's rest builds the second file's diff.
    host.fire(0);
    assert_eq!(host.model().preview, None);
    assert_ne!(host.model().rows.rows.len(), 0, "the diff is built");
    assert_eq!(host.model().rows.files[0].path, second_path);
}

/// A mark that lands while a file shows by its header alone keeps the header alone, and once
/// the cursor rests the file opens at its first hunk that needs the reviewer.
#[test]
fn a_mark_landing_during_a_preview_keeps_the_header_alone() {
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

    // Back in the tree, a held key moves on to the engine, which shows its header alone.
    host.dispatch(Action::FocusTree);
    let engine_position = host
        .model()
        .visible_tree()
        .iter()
        .position(|&index| host.model().tree[index].path == engine)
        .unwrap();
    host.update(|model| model.tree_cursor = engine_position - 1);
    hold_down(&mut host);
    assert_eq!(host.model().selected.as_deref(), Some(engine));
    assert!(host.model().preview.is_some());

    // The mark lands: the tree takes it in, and the engine still shows its header alone.
    let saved = host.run_job();
    host.complete(saved);
    assert!(!host.model().marking);
    assert!(host.model().preview.is_some());
    assert!(host.model().rows.rows.is_empty(), "no diff is built early");
    assert_eq!(host.model().rows.files[0].path, engine);

    // The cursor rests: the engine opens at its first hunk that needs the reviewer.
    host.fire(0);
    let model = host.model();
    assert_eq!(model.preview, None);
    assert_eq!(model.rows.files[0].path, engine);
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

/// The central check drops a rest for a preview that a newer move superseded, so the newest
/// preview keeps its header alone; the rest for the newest preview builds its diff.
#[test]
fn a_rest_for_an_older_preview_is_dropped_centrally() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let [start, ..] = three_files(host.model());
    let start_path = path_at(host.model(), start);
    host.update(|model| model.show_file(&start_path));
    hold_down(&mut host);
    let newest = host.model().preview.unwrap();

    host.complete(Completion {
        subject: Subject::Preview(newest - 1),
        outcome: Box::new(Outcome::Rested),
    });
    assert_eq!(host.model().preview, Some(newest));
    assert!(host.model().rows.rows.is_empty(), "no diff is built");

    host.complete(Completion {
        subject: Subject::Preview(newest),
        outcome: Box::new(Outcome::Rested),
    });
    assert_eq!(host.model().preview, None);
    assert_ne!(host.model().rows.rows.len(), 0, "the diff is built");
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

/// The central check drops a file read to open as a buffer for another revision than shows:
/// no buffer opens, though the read still ends; one read for the revision that shows opens.
#[test]
fn a_file_read_for_another_revision_is_dropped_centrally() {
    let dir = tempfile::tempdir().unwrap();
    let mut host = Host::open(demo(dir.path()));
    let shown = host.model().snapshot.as_ref().unwrap().revision;
    let other = RevisionNumber::new(shown.get() + 1).unwrap();
    let read = |revision| Completion {
        subject: Subject::Revision(revision),
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
        revision: model.snapshot.as_ref().unwrap().revision,
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
        lines: rows::FullText::new("// ledger\nfn balance() {}\n"),
        syntax: crate::syntax::analyze("src/ledger.rs", "// ledger\nfn balance() {}\n"),
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

    // An answer for another revision of the shown buffer is dropped too.
    let current = lookup(host.model());
    let other = Lookup {
        revision: RevisionNumber::new(current.revision.get() + 1).unwrap(),
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
        "tools/stats.py"
    );
    assert_eq!(shown(host.model()), "tools/stats.py", "the file previews");
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
    host.update(|model| model.show_file("Cargo.toml"));
    host.dispatch(Action::NextFile);
    assert_eq!(shown(host.model()), "Cargo.toml");
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
