//! The review window: a sidebar of steps, the selected step's rows, and a source pane.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    App, Context, FocusHandle, FontWeight, HighlightStyle, Hsla, KeyBinding, ScrollStrategy,
    SharedString, StyledText, UniformListScrollHandle, Window, actions, div, hsla, point, px,
    uniform_list,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};
use parking_lot::Mutex;
use review_core::walkthrough::{self, Partition, StepProgress};
use review_core::{History, MarkVerdict, Review, UnitId};

use crate::lsp::{self, LanguageServer};
use crate::rows::{self, Badge, HeadLine, Layout, Row, RowKind};

mod qa;

actions!(
    review_lite,
    [
        NextLine,
        PreviousLine,
        NextUnit,
        PreviousUnit,
        NextUnreviewed,
        NextStep,
        PreviousStep,
        NextToken,
        PreviousToken,
        MarkSeen,
        MarkAccepted,
        MarkFlagged,
        MarkUnseen,
        MarkHunkSeen,
        MarkHunkAccepted,
        MarkHunkFlagged,
        MarkHunkUnseen,
        GoToDefinition,
        ToggleTrust,
        CloseSource,
        Quit,
    ]
);

/// The key context of the review window.
const CONTEXT: &str = "ReviewApp";

/// The height of every row; the list is uniform so it only lays out visible rows.
const ROW_HEIGHT: f32 = 20.0;

/// How far the scroll benchmark moves per frame: thirty rows, a fast fling.
const BENCH_STEP: f32 = ROW_HEIGHT * 30.0;

/// How long go-to-definition waits for a language server that is still indexing.
const DEFINITION_DEADLINE: Duration = Duration::from_secs(120);

/// Binds the review window's keys.
pub fn bind_keys(cx: &mut App) {
    let context = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("j", NextLine, context),
        KeyBinding::new("down", NextLine, context),
        KeyBinding::new("k", PreviousLine, context),
        KeyBinding::new("up", PreviousLine, context),
        KeyBinding::new("n", NextUnit, context),
        KeyBinding::new("p", PreviousUnit, context),
        KeyBinding::new("u", NextUnreviewed, context),
        KeyBinding::new("]", NextStep, context),
        KeyBinding::new("[", PreviousStep, context),
        KeyBinding::new("l", NextToken, context),
        KeyBinding::new("right", NextToken, context),
        KeyBinding::new("h", PreviousToken, context),
        KeyBinding::new("left", PreviousToken, context),
        KeyBinding::new("s", MarkSeen, context),
        KeyBinding::new("a", MarkAccepted, context),
        KeyBinding::new("f", MarkFlagged, context),
        KeyBinding::new("x", MarkUnseen, context),
        KeyBinding::new("shift-s", MarkHunkSeen, context),
        KeyBinding::new("shift-a", MarkHunkAccepted, context),
        KeyBinding::new("shift-f", MarkHunkFlagged, context),
        KeyBinding::new("shift-x", MarkHunkUnseen, context),
        KeyBinding::new("g", GoToDefinition, context),
        KeyBinding::new("shift-t", ToggleTrust, context),
        KeyBinding::new("f12", GoToDefinition, context),
        KeyBinding::new("escape", CloseSource, context),
        KeyBinding::new("q", Quit, context),
        KeyBinding::new("ctrl-q", Quit, context),
    ]);
}

/// What the app does once its window is up, besides waiting for the reviewer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Startup {
    /// Scroll through every row once, print frame statistics, and quit.
    pub bench_scroll: bool,
    /// Quit once the first frame is rendered.
    pub exit_after_first_frame: bool,
    /// Read QA commands from standard input; see the `qa` module.
    pub qa: bool,
}

/// Whether a mark covers the focused unit or its whole display hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkScope {
    Unit,
    Hunk,
}

/// The engine's answers for one history, laid out for the window.
struct Loaded {
    history: Arc<History>,
    partition: Partition,
    progress: Vec<StepProgress>,
    rows: Layout,
}

/// Derives the partition, progress and rows of `history`'s current revision for the step
/// `selected`, or for every step when it is `None`.
fn load(history: Arc<History>, selected: Option<usize>) -> Result<Loaded, review_core::Error> {
    let (partition, progress, rows) = {
        let view = history.current_view()?;
        let partition = walkthrough::partition(&view);
        let progress = partition.progress(&view);
        let units = step_units(&partition, selected);
        let rows = rows::build(&view, &units);
        (partition, progress, rows)
    };
    Ok(Loaded {
        history,
        partition,
        progress,
        rows,
    })
}

/// The units of step `selected` in review order, or of every step.
fn step_units(partition: &Partition, selected: Option<usize>) -> Vec<UnitId> {
    match selected.and_then(|index| partition.steps.get(index)) {
        Some(step) => step.units.clone(),
        None => partition.order(),
    }
}

/// A read-only view of a file in the head worktree, opened by go-to-definition.
struct SourceView {
    title: SharedString,
    lines: Vec<SharedString>,
    target: usize,
}

/// The scroll benchmark's progress.
struct Bench {
    started: Instant,
    last: Instant,
    /// Time between consecutive frames, including waiting for the compositor.
    intervals: Vec<Duration>,
    /// When the view last started rendering.
    render_started: Option<Instant>,
    /// Time from the start of rendering until the visible rows were built in the frame just
    /// drawn: the app's own work, without gpui's paint and the compositor.
    built: Option<Duration>,
    /// `built` for every frame.
    work: Vec<Duration>,
    rendered_rows: usize,
}

/// Language servers by program and workspace root, started on first use and shared by
/// background tasks.
type Servers = Arc<Mutex<HashMap<(&'static str, PathBuf), LanguageServer>>>;

/// The review window.
pub struct ReviewApp {
    review: Arc<Review>,
    history: Arc<History>,
    partition: Partition,
    progress: Vec<StepProgress>,
    selected: Option<usize>,
    rows: Layout,
    cursor: usize,
    token: Option<Range<usize>>,
    list: UniformListScrollHandle,
    focus: FocusHandle,
    status: SharedString,
    busy: bool,
    looking_up: bool,
    /// Whether language servers may run on the reviewed code; see [`Review::trust`].
    trusted: bool,
    /// Whether `T` was pressed once and trusting awaits a second press.
    trust_pending: bool,
    source: Option<SourceView>,
    source_list: UniformListScrollHandle,
    servers: Servers,
    bench: Option<Bench>,
}

impl ReviewApp {
    /// Lays out the current revision of `history`, which was loaded from `review`.
    pub fn new(
        review: Review,
        history: History,
        startup: Startup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let history = Arc::new(history);
        let (partition, progress, rows, status) = match load(Arc::clone(&history), None) {
            Ok(loaded) => (loaded.partition, loaded.progress, loaded.rows, None),
            Err(err) => (
                Partition {
                    revision: review_core::RevisionNumber::FIRST,
                    title: None,
                    written_for: None,
                    steps: Vec::new(),
                    warnings: Vec::new(),
                },
                Vec::new(),
                Layout::default(),
                Some(format!("cannot show the review: {err}")),
            ),
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut app = Self {
            review: Arc::new(review),
            history,
            partition,
            progress,
            selected: None,
            rows,
            cursor: 0,
            token: None,
            list: UniformListScrollHandle::new(),
            focus,
            status: SharedString::default(),
            busy: false,
            looking_up: false,
            trusted: false,
            trust_pending: false,
            source: None,
            source_list: UniformListScrollHandle::new(),
            servers: Servers::default(),
            bench: None,
        };
        let trust = app.review.trust();
        app.trusted = trust.as_ref().is_ok_and(|trust| trust.trusted);
        let status = status.or_else(|| {
            trust
                .err()
                .map(|err| format!("cannot read whether the review is trusted: {err}"))
        });
        app.status = status.unwrap_or_else(|| app.summary()).into();
        app.reset_token();
        if startup.qa {
            qa::start(window, cx);
        }
        if startup.bench_scroll {
            cx.on_next_frame(window, ReviewApp::start_bench);
        } else if startup.exit_after_first_frame {
            cx.on_next_frame(window, |this, _window, cx| {
                println!(
                    "first frame rendered: {} steps, {} rows",
                    this.partition.steps.len(),
                    this.rows.rows.len()
                );
                cx.quit();
            });
        }
        app
    }

    /// The status line shown when nothing else needs saying.
    fn summary(&self) -> String {
        let (reviewed, total) = self
            .progress
            .iter()
            .fold((0, 0), |(reviewed, total), step| {
                (reviewed + step.reviewed, total + step.total)
            });
        let mut text = format!(
            "revision {}: {reviewed}/{total} units reviewed",
            self.partition.revision
        );
        if !self.trusted {
            text = format!("{text}; untrusted, language servers off (T to trust)");
        }
        match self.history.warnings().first() {
            Some(warning) => format!("{text}; warning: {warning}"),
            None => text,
        }
    }

    fn row(&self) -> Option<&Row> {
        self.rows.rows.get(self.cursor)
    }

    fn focused_unit(&self) -> Option<UnitId> {
        self.row().map(|row| row.unit)
    }

    /// Selects step `selected` (or every step) and rebuilds its rows from the engine.
    fn select_step(&mut self, selected: Option<usize>, cx: &mut Context<Self>) {
        self.trust_pending = false;
        self.selected = selected;
        match self.history.current_view() {
            Ok(view) => {
                let units = step_units(&self.partition, selected);
                self.rows = rows::build(&view, &units);
            }
            Err(err) => self.status = format!("cannot show the step: {err}").into(),
        }
        self.cursor = 0;
        self.list.scroll_to_item(0, ScrollStrategy::Top);
        self.reset_token();
        cx.notify();
    }

    /// Moves the cursor to `row` and scrolls it into view.
    fn move_cursor(&mut self, row: usize, cx: &mut Context<Self>) {
        self.trust_pending = false;
        let Some(last) = self.rows.rows.len().checked_sub(1) else {
            return;
        };
        self.cursor = row.min(last);
        self.list
            .scroll_to_item(self.cursor, ScrollStrategy::Nearest);
        self.reset_token();
        cx.notify();
    }

    /// Moves the cursor to `unit`'s header, switching steps when another step holds it.
    fn focus_unit(&mut self, unit: UnitId, cx: &mut Context<Self>) {
        if !self.rows.unit_rows.contains_key(&unit) {
            let step = self.partition.step_of(unit);
            self.select_step(step, cx);
        }
        if let Some(&row) = self.rows.unit_rows.get(&unit) {
            self.cursor = row;
            self.list.scroll_to_item(row, ScrollStrategy::Top);
            self.reset_token();
            cx.notify();
        }
    }

    fn next_line(&mut self, _: &NextLine, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(self.cursor + 1, cx);
    }

    fn previous_line(&mut self, _: &PreviousLine, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cursor(self.cursor.saturating_sub(1), cx);
    }

    fn step_unit(&mut self, offset: isize, cx: &mut Context<Self>) {
        if let Some(unit) = self.partition.neighbor(self.focused_unit(), offset) {
            self.focus_unit(unit, cx);
        }
    }

    fn next_unit(&mut self, _: &NextUnit, _: &mut Window, cx: &mut Context<Self>) {
        self.step_unit(1, cx);
    }

    fn previous_unit(&mut self, _: &PreviousUnit, _: &mut Window, cx: &mut Context<Self>) {
        self.step_unit(-1, cx);
    }

    fn next_unreviewed(&mut self, _: &NextUnreviewed, _: &mut Window, cx: &mut Context<Self>) {
        let found = self
            .history
            .current_view()
            .ok()
            .and_then(|view| self.partition.next_unreviewed(&view, self.focused_unit()));
        if let Some(unit) = found {
            self.focus_unit(unit, cx);
        } else {
            self.status = "every unit is reviewed".into();
            cx.notify();
        }
    }

    fn change_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let count = self.partition.steps.len();
        let selected = match (self.selected, forward) {
            (None, true) => (count > 0).then_some(0),
            (None, false) => count.checked_sub(1),
            (Some(index), true) => (index + 1 < count).then_some(index + 1),
            (Some(index), false) => index.checked_sub(1),
        };
        self.select_step(selected, cx);
    }

    fn next_step(&mut self, _: &NextStep, _: &mut Window, cx: &mut Context<Self>) {
        self.change_step(true, cx);
    }

    fn previous_step(&mut self, _: &PreviousStep, _: &mut Window, cx: &mut Context<Self>) {
        self.change_step(false, cx);
    }

    /// Puts the token cursor on the first identifier of the cursor row, if it is a head line.
    fn reset_token(&mut self) {
        self.token = self
            .row()
            .filter(|row| row.head.is_some())
            .and_then(|row| identifiers(&row.text).into_iter().next());
    }

    fn move_token(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(row) = self.row().filter(|row| row.head.is_some()) else {
            return;
        };
        let tokens = identifiers(&row.text);
        let start = self.token.as_ref().map_or(0, |token| token.start);
        let next = if forward {
            tokens.iter().find(|token| token.start > start)
        } else {
            tokens.iter().rev().find(|token| token.start < start)
        };
        if let Some(next) = next.cloned() {
            self.token = Some(next);
            cx.notify();
        }
    }

    fn next_token(&mut self, _: &NextToken, _: &mut Window, cx: &mut Context<Self>) {
        self.move_token(true, cx);
    }

    fn previous_token(&mut self, _: &PreviousToken, _: &mut Window, cx: &mut Context<Self>) {
        self.move_token(false, cx);
    }

    /// Marks the focused unit or hunk through the engine, then reloads the history in the
    /// background and refreshes the view from it.
    fn mark(&mut self, verdict: MarkVerdict, scope: MarkScope, cx: &mut Context<Self>) {
        self.trust_pending = false;
        if self.busy {
            self.status = "still saving the previous mark".into();
            cx.notify();
            return;
        }
        let Some(row) = self.row() else {
            return;
        };
        let units = match scope {
            MarkScope::Unit => vec![row.unit],
            MarkScope::Hunk => self.rows.hunks.get(row.hunk).cloned().unwrap_or_default(),
        };
        let anchor = (
            row.unit,
            self.cursor
                .saturating_sub(self.rows.unit_rows.get(&row.unit).copied().unwrap_or(0)),
        );
        let review = Arc::clone(&self.review);
        let history = Arc::clone(&self.history);
        let selected = self.selected;
        let count = units.len();
        self.busy = true;
        self.status = format!("marking {count} unit(s) {}…", verdict.name()).into();
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            {
                let view = history.current_view()?;
                review.mark(&view, &units, verdict)?;
            }
            load(Arc::new(review.history()?), selected)
        });
        cx.spawn(async move |this, cx| {
            let loaded = work.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match loaded {
                    Ok(loaded) => {
                        this.apply(loaded, anchor);
                        this.status = format!(
                            "marked {count} unit(s) {}; {}",
                            verdict.name(),
                            this.summary()
                        )
                        .into();
                    }
                    Err(err) => this.status = format!("mark failed: {err}").into(),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Replaces the engine state, keeping the cursor at the same line of the same unit.
    fn apply(&mut self, loaded: Loaded, (unit, offset): (UnitId, usize)) {
        self.history = loaded.history;
        self.partition = loaded.partition;
        self.progress = loaded.progress;
        self.rows = loaded.rows;
        let last = self.rows.rows.len().saturating_sub(1);
        self.cursor = self
            .rows
            .unit_rows
            .get(&unit)
            .map_or(0, |&row| row + offset)
            .min(last);
        if self.row().map(|row| row.unit) != Some(unit) {
            self.cursor = self
                .rows
                .unit_rows
                .get(&unit)
                .copied()
                .unwrap_or(0)
                .min(last);
        }
        self.reset_token();
    }

    fn mark_seen(&mut self, _: &MarkSeen, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Seen, MarkScope::Unit, cx);
    }

    fn mark_accepted(&mut self, _: &MarkAccepted, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Accepted, MarkScope::Unit, cx);
    }

    fn mark_flagged(&mut self, _: &MarkFlagged, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Flagged, MarkScope::Unit, cx);
    }

    fn mark_unseen(&mut self, _: &MarkUnseen, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Unseen, MarkScope::Unit, cx);
    }

    fn mark_hunk_seen(&mut self, _: &MarkHunkSeen, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Seen, MarkScope::Hunk, cx);
    }

    fn mark_hunk_accepted(&mut self, _: &MarkHunkAccepted, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Accepted, MarkScope::Hunk, cx);
    }

    fn mark_hunk_flagged(&mut self, _: &MarkHunkFlagged, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Flagged, MarkScope::Hunk, cx);
    }

    fn mark_hunk_unseen(&mut self, _: &MarkHunkUnseen, _: &mut Window, cx: &mut Context<Self>) {
        self.mark(MarkVerdict::Unseen, MarkScope::Hunk, cx);
    }

    /// Asks a language server, in the background, where the identifier under the token cursor
    /// is defined, and opens that file read-only.
    fn go_to_definition(&mut self, _: &GoToDefinition, _: &mut Window, cx: &mut Context<Self>) {
        self.trust_pending = false;
        if !self.trusted {
            self.status = "go-to-definition needs a language server, which runs code from this \
                           review; press T to trust it first"
                .into();
            cx.notify();
            return;
        }
        let (Some(head), Some(token)) = (
            self.row().and_then(|row| row.head.clone()),
            self.token.clone(),
        ) else {
            self.status =
                "go-to-definition starts from an added line; move to one with j and k".into();
            cx.notify();
            return;
        };
        let Some(record) = self
            .history
            .revisions()
            .last()
            .map(|record| (*record).clone())
        else {
            return;
        };
        let review = Arc::clone(&self.review);
        let servers = Arc::clone(&self.servers);
        self.looking_up = true;
        self.status = "looking up the definition (the language server may need to index first)…"
            .to_string()
            .into();
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            let root = review.worktree(&record).map_err(|err| err.to_string())?;
            find_definition(&servers, &root, &head, token.start)
        });
        cx.spawn(async move |this, cx| {
            let found = work.await;
            this.update(cx, |this, cx| {
                this.looking_up = false;
                match found {
                    Ok(source) => {
                        this.source_list
                            .scroll_to_item(source.target, ScrollStrategy::Center);
                        this.status = format!("{}  (escape closes)", source.title).into();
                        this.source = Some(source);
                    }
                    Err(err) => this.status = format!("go-to-definition: {err}").into(),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Trusts the review after a confirming second press, or revokes trust and stops the
    /// language servers.
    fn toggle_trust(&mut self, _: &ToggleTrust, _: &mut Window, cx: &mut Context<Self>) {
        let decision = match (self.trusted, self.trust_pending) {
            (true, _) => false,
            (false, true) => true,
            (false, false) => {
                self.trust_pending = true;
                self.status = "trusting runs rust-analyzer in a worktree of the reviewed head, \
                               and through it cargo metadata and rustc under the review's own \
                               Cargo configuration, which can run any program; press T again to \
                               trust"
                    .into();
                cx.notify();
                return;
            }
        };
        self.trust_pending = false;
        if self.looking_up {
            self.status = "wait for the definition lookup to finish".into();
            cx.notify();
            return;
        }
        match self.review.set_trusted(decision) {
            Ok(()) => {
                self.trusted = decision;
                if !decision {
                    // Dropping a server shuts it down.
                    self.servers.lock().clear();
                }
                let done = if decision { "trusted" } else { "trust revoked" };
                self.status = format!("{done}; {}", self.summary()).into();
            }
            Err(err) => self.status = format!("cannot record trust: {err}").into(),
        }
        cx.notify();
    }

    fn close_source(&mut self, _: &CloseSource, _: &mut Window, cx: &mut Context<Self>) {
        self.trust_pending = false;
        self.source = None;
        self.status = self.summary().into();
        cx.notify();
    }

    fn start_bench(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_some() {
            self.select_step(None, cx);
        }
        let now = Instant::now();
        self.bench = Some(Bench {
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

    /// Records the interval since the last frame and scrolls one step further, or reports and
    /// quits at the end of the list.
    fn bench_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bench) = &mut self.bench else {
            return;
        };
        let now = Instant::now();
        bench.intervals.push(now - bench.last);
        bench.last = now;
        if let Some(built) = bench.built.take() {
            bench.work.push(built);
        }
        let handle = self.list.0.borrow().base_handle.clone();
        let max = handle.max_offset().y;
        let current = -handle.offset().y;
        if bench.intervals.len() > 1 && current >= max {
            println!("{}", bench_report(bench, self.rows.rows.len()));
            cx.quit();
            return;
        }
        handle.set_offset(point(px(0.0), -(current + px(BENCH_STEP)).min(max)));
        cx.notify();
        cx.on_next_frame(window, ReviewApp::bench_frame);
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<impl IntoElement + use<>> {
        let colors = Palette::dark();
        let theme = cx.theme();
        let mono = theme.mono_font_family.clone();
        let focused_hunk = self.row().map(|row| row.hunk);
        let rows: Vec<_> = range
            .filter_map(|index| {
                let row = self.rows.rows.get(index)?;
                let token = (index == self.cursor).then(|| self.token.clone()).flatten();
                Some(
                    render_row(
                        index,
                        row,
                        index == self.cursor,
                        Some(row.hunk) == focused_hunk,
                        token,
                        &colors,
                    )
                    .font_family(mono.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.cursor = index;
                        this.reset_token();
                        cx.notify();
                    })),
                )
            })
            .collect();
        if let Some(bench) = &mut self.bench {
            bench.rendered_rows += rows.len();
            bench.built = bench.render_started.map(|started| started.elapsed());
        }
        rows
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (active, foreground, muted, done) = (
            theme.list_active,
            theme.foreground,
            theme.muted_foreground,
            Palette::dark().accepted,
        );
        let total = self.progress.iter().fold(
            StepProgress {
                reviewed: 0,
                total: 0,
            },
            |sum, step| StepProgress {
                reviewed: sum.reviewed + step.reviewed,
                total: sum.total + step.total,
            },
        );
        let entries = std::iter::once((None, "All changes".to_owned(), total)).chain(
            self.partition
                .steps
                .iter()
                .zip(&self.progress)
                .enumerate()
                .map(|(index, (step, progress))| (Some(index), step.title.clone(), *progress)),
        );
        let title = self
            .partition
            .title
            .clone()
            .unwrap_or_else(|| "Files".to_owned());
        v_flex()
            .id("steps")
            .w(px(300.0))
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                div()
                    .px_2()
                    .py_1()
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            )
            .children(entries.map(|(index, title, progress)| {
                let selected = index == self.selected;
                h_flex()
                    .id(SharedString::from(format!("step-{index:?}")))
                    .px_2()
                    .py_0p5()
                    .gap_2()
                    .when(selected, |row| row.bg(active))
                    .text_color(foreground)
                    .child(div().flex_1().truncate().child(title))
                    .child(
                        div()
                            .text_color(if progress.reviewed == progress.total {
                                done
                            } else {
                                muted
                            })
                            .child(format!("{}/{}", progress.reviewed, progress.total)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select_step(index, cx)))
            }))
    }

    /// The selected step's title and why it matters.
    fn render_step_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (title, why) = match self
            .selected
            .and_then(|index| self.partition.steps.get(index))
        {
            Some(step) => (step.title.clone(), step.why.clone()),
            None => ("All changes".to_owned(), String::new()),
        };
        v_flex()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(div().font_weight(FontWeight::BOLD).truncate().child(title))
            .when(!why.is_empty(), |header| {
                header.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(why),
                )
            })
    }

    fn render_source(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let source = self.source.as_ref()?;
        let theme = cx.theme();
        let (target_bg, muted, mono) = (
            theme.selection,
            theme.muted_foreground,
            theme.mono_font_family.clone(),
        );
        let lines = source.lines.clone();
        let target = source.target;
        Some(
            v_flex()
                .flex_1()
                .h_full()
                .border_l_1()
                .border_color(theme.border)
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .font_weight(FontWeight::BOLD)
                        .truncate()
                        .child(source.title.clone()),
                )
                .child(
                    uniform_list("source", lines.len(), move |range, _, _| {
                        range
                            .filter_map(|index| {
                                let line = lines.get(index)?.clone();
                                Some(
                                    h_flex()
                                        .h(px(ROW_HEIGHT))
                                        .px_2()
                                        .font_family(mono.clone())
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .w_full()
                                        .when(index == target, |row| row.bg(target_bg))
                                        .child(
                                            div()
                                                .w(px(56.0))
                                                .flex_none()
                                                .text_color(muted)
                                                .child(format!("{:>5}", index + 1)),
                                        )
                                        .child(line),
                                )
                            })
                            .collect()
                    })
                    .track_scroll(&self.source_list)
                    .flex_1(),
                ),
        )
    }
}

/// Colors of the hunk list, tuned for a dark background: muted line backgrounds, stronger
/// backgrounds for changed tokens, and distinct text colors for moves, collapsed units,
/// interdiffs and review state badges.
struct Palette {
    removed_line: Hsla,
    removed_token: Hsla,
    removed_cursor: Hsla,
    added_line: Hsla,
    added_token: Hsla,
    added_cursor: Hsla,
    header: Hsla,
    header_cursor: Hsla,
    plain_cursor: Hsla,
    hunk_border: Hsla,
    cursor_border: Hsla,
    token: Hsla,
    gutter: Hsla,
    info: Hsla,
    moved: Hsla,
    collapsed: Hsla,
    interdiff: Hsla,
    note: Hsla,
    unreviewed: Hsla,
    seen: Hsla,
    accepted: Hsla,
    flagged: Hsla,
    changed: Hsla,
}

impl Palette {
    fn dark() -> Self {
        // Hues are fractions of a turn, as gpui's `hsla` takes them.
        let color =
            |degrees: f32, saturation, lightness| hsla(degrees / 360.0, saturation, lightness, 1.0);
        Self {
            removed_line: color(0.0, 0.30, 0.17),
            removed_token: color(0.0, 0.50, 0.33),
            removed_cursor: color(0.0, 0.30, 0.24),
            added_line: color(140.0, 0.28, 0.15),
            added_token: color(140.0, 0.45, 0.29),
            added_cursor: color(140.0, 0.28, 0.22),
            header: color(220.0, 0.14, 0.17),
            header_cursor: color(215.0, 0.28, 0.26),
            plain_cursor: color(215.0, 0.28, 0.22),
            hunk_border: color(210.0, 0.55, 0.45),
            cursor_border: color(45.0, 0.95, 0.60),
            token: color(210.0, 0.70, 0.42),
            gutter: color(220.0, 0.08, 0.45),
            info: color(220.0, 0.10, 0.62),
            moved: color(190.0, 0.55, 0.65),
            collapsed: color(265.0, 0.35, 0.72),
            interdiff: color(32.0, 0.85, 0.65),
            note: color(48.0, 0.80, 0.65),
            unreviewed: color(210.0, 0.85, 0.70),
            seen: color(150.0, 0.25, 0.62),
            accepted: color(140.0, 0.60, 0.58),
            flagged: color(8.0, 0.80, 0.64),
            changed: color(40.0, 0.95, 0.58),
        }
    }

    fn badge(&self, badge: Badge) -> Hsla {
        match badge {
            Badge::Unreviewed => self.unreviewed,
            Badge::Seen => self.seen,
            Badge::Accepted => self.accepted,
            Badge::Flagged => self.flagged,
            Badge::Changed => self.changed,
        }
    }
}

/// Renders one row of the hunk list.
fn render_row(
    index: usize,
    row: &Row,
    is_cursor: bool,
    in_focused_hunk: bool,
    token: Option<Range<usize>>,
    colors: &Palette,
) -> gpui::Stateful<gpui::Div> {
    let (sign, background, cursor_background, strong) = match row.kind {
        RowKind::Removed | RowKind::NestedOld => (
            "-",
            Some(colors.removed_line),
            colors.removed_cursor,
            colors.removed_token,
        ),
        RowKind::Added | RowKind::NestedNew => (
            "+",
            Some(colors.added_line),
            colors.added_cursor,
            colors.added_token,
        ),
        RowKind::Header => ("", Some(colors.header), colors.header_cursor, colors.header),
        RowKind::NestedSame
        | RowKind::Info
        | RowKind::Moved
        | RowKind::Collapsed
        | RowKind::Interdiff
        | RowKind::Note => (" ", None, colors.plain_cursor, colors.header),
    };
    let foreground = match row.kind {
        RowKind::Info => Some(colors.info),
        RowKind::Moved => Some(colors.moved),
        RowKind::Collapsed => Some(colors.collapsed),
        RowKind::Interdiff => Some(colors.interdiff),
        RowKind::Note => Some(colors.note),
        RowKind::Header
        | RowKind::Removed
        | RowKind::Added
        | RowKind::NestedOld
        | RowKind::NestedNew
        | RowKind::NestedSame => None,
    };
    let highlights = row_highlights(row, token, strong, colors);
    let nested = matches!(
        row.kind,
        RowKind::NestedOld | RowKind::NestedNew | RowKind::NestedSame
    );
    let gutter = match &row.head {
        Some(head) => format!("{:>5}", head.line + 1),
        // A rule in the gutter sets nested diffs apart from the unit's own lines.
        None if nested => "    ┆".to_owned(),
        None => String::new(),
    };
    let text = StyledText::new(row.text.clone()).with_highlights(highlights);
    let border = if is_cursor {
        colors.cursor_border
    } else if in_focused_hunk {
        colors.hunk_border
    } else {
        gpui::transparent_black()
    };
    h_flex()
        .id(("row", index))
        .h(px(ROW_HEIGHT))
        .w_full()
        .whitespace_nowrap()
        .overflow_hidden()
        .border_l_4()
        .border_color(border)
        .when_some(background, gpui::Styled::bg)
        .when(is_cursor, |div| div.bg(cursor_background))
        .when(row.kind == RowKind::Header, |div| {
            div.font_weight(FontWeight::BOLD)
        })
        .when_some(foreground, gpui::Styled::text_color)
        .when(
            matches!(
                row.kind,
                RowKind::Moved | RowKind::Collapsed | RowKind::Info
            ),
            gpui::Styled::italic,
        )
        .child(
            div()
                .w(px(48.0))
                .flex_none()
                .text_color(if nested {
                    colors.interdiff
                } else {
                    colors.gutter
                })
                .child(gutter),
        )
        .child(div().w(px(16.0)).flex_none().child(sign))
        .child(text)
}

/// The highlights of a row: changed tokens in `strong`, the go-to-definition token, and the
/// review state badge, in text order.
fn row_highlights(
    row: &Row,
    token: Option<Range<usize>>,
    strong: Hsla,
    colors: &Palette,
) -> Vec<(Range<usize>, HighlightStyle)> {
    // Token highlights start after the indentation, which would otherwise light up the whole
    // left edge of a rewritten block.
    let indent = row.text.len() - row.text.trim_start().len();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = row
        .highlights
        .iter()
        .map(|range| range.start.max(indent)..range.end)
        .filter(|range| !range.is_empty())
        .filter(|range| {
            token
                .as_ref()
                .is_none_or(|token| range.end <= token.start || range.start >= token.end)
        })
        .map(|range| {
            (
                range.clone(),
                HighlightStyle {
                    background_color: Some(strong),
                    ..HighlightStyle::default()
                },
            )
        })
        .collect();
    if let Some(token) = token {
        highlights.push((
            token,
            HighlightStyle {
                background_color: Some(colors.token),
                ..HighlightStyle::default()
            },
        ));
    }
    if let Some((badge, range)) = row.badge.zip(badge_range(&row.text)) {
        highlights.push((
            range,
            HighlightStyle {
                color: Some(colors.badge(badge)),
                ..HighlightStyle::default()
            },
        ));
    }
    highlights.sort_by_key(|(range, _)| range.start);
    highlights
}

/// The bracketed badge in a header row's text.
fn badge_range(text: &str) -> Option<Range<usize>> {
    let start = text.find('[')?;
    let end = start + text.get(start..)?.find(']')? + 1;
    Some(start..end)
}

impl Render for ReviewApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(bench) = &mut self.bench {
            bench.render_started = Some(Instant::now());
        }
        let theme = cx.theme();
        let (background, foreground, border, status_bg) = (
            theme.background,
            theme.foreground,
            theme.border,
            theme.status_bar,
        );
        let font_size = px(13.0);
        let sidebar = self.render_sidebar(cx);
        let step_header = self.render_step_header(cx);
        let source = self.render_source(cx);
        let list = uniform_list(
            "rows",
            self.rows.rows.len(),
            cx.processor(|this, range, _, cx| this.render_rows(range, cx)),
        )
        .track_scroll(&self.list)
        .flex_1()
        .h_full();
        v_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::next_line))
            .on_action(cx.listener(Self::previous_line))
            .on_action(cx.listener(Self::next_unit))
            .on_action(cx.listener(Self::previous_unit))
            .on_action(cx.listener(Self::next_unreviewed))
            .on_action(cx.listener(Self::next_step))
            .on_action(cx.listener(Self::previous_step))
            .on_action(cx.listener(Self::next_token))
            .on_action(cx.listener(Self::previous_token))
            .on_action(cx.listener(Self::mark_seen))
            .on_action(cx.listener(Self::mark_accepted))
            .on_action(cx.listener(Self::mark_flagged))
            .on_action(cx.listener(Self::mark_unseen))
            .on_action(cx.listener(Self::mark_hunk_seen))
            .on_action(cx.listener(Self::mark_hunk_accepted))
            .on_action(cx.listener(Self::mark_hunk_flagged))
            .on_action(cx.listener(Self::mark_hunk_unseen))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::toggle_trust))
            .on_action(cx.listener(Self::close_source))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .size_full()
            .bg(background)
            .text_color(foreground)
            .text_size(font_size)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(sidebar)
                    .child(v_flex().flex_1().h_full().min_w_0().child(step_header).child(list))
                    .children(source),
            )
            .child(
                div()
                    .px_2()
                    .py_0p5()
                    .border_t_1()
                    .border_color(border)
                    .bg(status_bg)
                    .truncate()
                    .child(self.status.clone()),
            )
            .child(
                div().px_2().pb_0p5().bg(status_bg).truncate().child(
                    "j/k line  n/p unit  u next unreviewed  [/] step  s/a/f/x mark unit (shift: hunk)  h/l token  g definition  T trust  esc close  q quit",
                ),
            )
    }
}

/// Byte ranges of the identifiers in `text`.
fn identifiers(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        let word = ch.is_alphanumeric() || ch == '_';
        match (word, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                found.push(begin..index);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        found.push(begin..text.len());
    }
    found.retain(|range| !text[range.clone()].starts_with(|ch: char| ch.is_ascii_digit()));
    found
}

/// Finds where the identifier at byte `column` of `head` is defined, starting a language server
/// for `root` on first use, and loads that file.
fn find_definition(
    servers: &Servers,
    root: &Path,
    head: &HeadLine,
    column: usize,
) -> Result<SourceView, String> {
    // Language servers answer with canonical paths, so the root must be canonical to strip it.
    let root = &root
        .canonicalize()
        .map_err(|err| format!("cannot open {}: {err}", root.display()))?;
    let path = root.join(&head.path);
    let command = lsp::server_for(&path)
        .ok_or_else(|| format!("no language server is configured for {}", head.path))?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", head.path))?;
    let line = text
        .lines()
        .nth(head.line)
        .ok_or_else(|| format!("{} has no line {}", head.path, head.line + 1))?;
    let position = lsp::Position {
        line: head.line,
        character: lsp::utf16_column(line, column.min(line.len())),
    };
    let mut servers = servers.lock();
    let key = (command.program, root.clone());
    if !servers.contains_key(&key) {
        let server = LanguageServer::start(&command, root).map_err(|err| err.to_string())?;
        servers.insert(key.clone(), server);
    }
    let server = servers.get_mut(&key).ok_or("the language server is gone")?;
    let definition = server
        .definition(&path, &text, position, DEFINITION_DEADLINE)
        .map_err(|err| err.to_string())?;
    drop(servers);
    let target = std::fs::read_to_string(&definition.path)
        .map_err(|err| format!("cannot read {}: {err}", definition.path.display()))?;
    let shown = definition
        .path
        .strip_prefix(root)
        .unwrap_or(&definition.path)
        .display()
        .to_string();
    Ok(SourceView {
        title: format!("{shown}:{} (read-only)", definition.position.line + 1).into(),
        lines: target
            .lines()
            .map(|line| SharedString::from(line.to_owned()))
            .collect(),
        target: definition.position.line,
    })
}

/// Summarizes the scroll benchmark's frame intervals and per-frame work.
fn bench_report(bench: &Bench, rows: usize) -> String {
    let intervals: Vec<Duration> = bench.intervals.iter().skip(1).copied().collect();
    format!(
        "scrolled {rows} rows in {} frames over {:.1?} ({} rows rendered)\n\
         frame interval: {}\n\
         render through row building: {}",
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
        "p50 {:.1?}, p95 {:.1?}, p99 {:.1?}, max {:.1?}; over 16.7 ms: {}, over 33.4 ms: {}",
        percentile(0.5),
        percentile(0.95),
        percentile(0.99),
        values.last().copied().unwrap_or_default(),
        over(Duration::from_micros(16_700)),
        over(Duration::from_micros(33_400)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_skip_numbers_and_punctuation() {
        let text = "let total = sum(a_b, 42) + café;";
        let found: Vec<&str> = identifiers(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["let", "total", "sum", "a_b", "café"]);
    }

    /// Follows a call on an added line of the demo into the head worktree; skipped where
    /// rust-analyzer is not installed.
    #[test]
    fn definition_from_a_demo_head_line() {
        if std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipped: rust-analyzer is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let review = review_core::demo::prepare(
            &review_core::demo::DemoPaths::under(dir.path()),
            &review_core::demo::fixture_dir(),
            review_core::demo::DemoRequest::Fresh,
        )
        .unwrap();
        let history = review.history().unwrap();
        let root = review
            .worktree(history.revisions().last().unwrap())
            .unwrap();
        let view = history.current_view().unwrap();
        let rows = rows::build(&view, &walkthrough::partition(&view).order());
        let row = rows
            .rows
            .iter()
            .find(|row| row.kind == RowKind::Added && row.text.contains("util::format_cents("))
            .unwrap();
        let head = row.head.as_ref().unwrap();
        assert_eq!(head.path, "src/format.rs");

        let column = row.text.find("format_cents").unwrap();
        let source = find_definition(&Servers::default(), &root, head, column).unwrap();
        assert!(source.title.starts_with("src/util.rs:"), "{}", source.title);
        assert!(source.lines[source.target].contains("fn format_cents"));
    }
}
