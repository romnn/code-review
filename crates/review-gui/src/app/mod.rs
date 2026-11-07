//! The review window: the gpui view over [`review_ui::model`].
//!
//! The model holds every piece of the window's state that is not a gpui handle, and every
//! behavior.
//! The view draws it, reading it through [`ReviewApp`]'s [`Deref`] to [`Model`], and changes it
//! only through [`ReviewApp::update_model`], which runs the model's after-update step, runs the
//! [`Effect`]s the model queued (background jobs on gpui's executor, timers, the clipboard, the
//! title) and brings the gpui side in line: the text fields' inputs and the focus.
//! Here live only what gpui needs: focus handles, the lists' scroll handles, the inputs of the
//! text fields, the key matcher and its timer, the text layouts the last frame painted, the
//! title bar's regions, the fonts and what the last frames measured, and the scroll benchmark
//! and latency samples.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Deref;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AppContext as _, ClipboardItem, Context, Entity, FocusHandle, ScrollHandle, Subscription,
    TextLayout, Window,
};
use gpui_component::input::{InputEvent, InputState};
use indoc::formatdoc;
use review_ui::frame::Grid;
use review_ui::keymap::Area;
use review_ui::model::{
    Action, Driver, Effect, Field, FieldState, Model, Opener, Overlay, Phase, Quitting, Settings,
    SheetStep, Updated, file_paths,
};
use review_ui::text::{self, Pane};

mod chrome;
mod ink;
mod keys;
mod measure;
mod overview;
mod pairs;
mod picker;
mod qa;
mod render;
mod scroll;
mod select;
mod settings;

pub use measure::Fonts;
use measure::Sizes;
use scroll::Lists;

/// How far the scroll benchmark moves per frame: thirty rows, a fast fling.
const BENCH_ROWS_PER_FRAME: f64 = 30.0;

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

/// The lines painted in the last frame, by pane, column and row, for mapping the mouse to
/// text.
type PaintedRows = Rc<RefCell<HashMap<(Pane, text::Column, usize), PaintedLine>>>;

/// A line of code as the last frame painted it.
#[derive(Clone)]
struct PaintedLine {
    /// Its shown text as gpui laid it out.
    layout: TextLayout,
    /// How its shown text maps back to the bytes of the line.
    line: Rc<review_ui::paint::display::DisplayLine>,
}

/// The gpui input of one of the model's text fields, opened as [`FieldState::id`].
struct Input {
    field: Field,
    id: u64,
    state: Entity<InputState>,
    /// Reports the input's edits and Enter to the model while the input lives.
    #[expect(
        dead_code,
        reason = "held for its drop, which ends the reports when the field closes"
    )]
    subscription: Subscription,
}

/// What has the keyboard: the handle of an area, or the input of a text field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Area(Area),
    Field(Field, u64),
}

/// The review window.
pub struct ReviewApp {
    driver: Driver,
    tree_focus: FocusHandle,
    diff_focus: FocusHandle,
    buffer_focus: FocusHandle,
    overlay_focus: FocusHandle,
    overview_scroll: ScrollHandle,
    help_scroll: ScrollHandle,
    /// The inputs of the open text fields.
    inputs: Vec<Input>,
    /// The key matcher, which every key press goes to.
    keys: keys::Keys,
    /// What the model gave the keyboard at the last update; focus moves only when the model
    /// moves it, so a click that focused something else stays in effect until then.
    focused: Focus,
    /// Whether the sidebar divider is being dragged.
    resizing: bool,
    /// Named hit regions and window operations, for the title bar and QA mode.
    regions: Rc<chrome::Regions>,
    client_decorated: bool,
    painted: PaintedRows,
    fonts: Fonts,
    /// The gpui handles of the scrolled lists, which mirror the model's offsets each frame.
    lists: Lists,
    /// The diff pane's width in logical pixels, which follows the window each frame.
    diff_width: f32,
    /// The code size the lists were last laid out at, which their bounds reflect.
    lists_code_size: f32,
    /// The code cell width in pixels the side-by-side text's extent was last worked out in,
    /// which turns the extent the model keeps in cells back into pixels for QA mode; the extent
    /// stays as it was while the diff shows unified.
    extent_cell: f32,
    /// The grid the last frame's layout phase ran against.
    laid_out: Option<Grid>,
    /// Whether gpui laid the last frame out in other viewports than the layout phase expected,
    /// so the next frame lays out again.
    relayout: bool,
    /// How wide the settings panel drew last, for QA mode's `state`.
    settings_width: Cell<f32>,
    bench: Option<Bench>,
    latency: Latency,
    startup: Startup,
    /// Whether the startup work waits for the review to open.
    starting: bool,
    /// How long the first hunk took to show, from process start.
    first_hunk: Option<Duration>,
    /// Whether QA mode is answering a command, so the app quits only once it has answered.
    answering: bool,
}

/// The view reads the model as its own state; it changes it only through
/// [`ReviewApp::update_model`].
impl Deref for ReviewApp {
    type Target = Model;

    fn deref(&self) -> &Model {
        self.driver.model()
    }
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
        let lists = Lists::new();
        let diff_width = measure::opening_width(
            &settings,
            fonts.advance,
            f32::from(window.viewport_size().width),
        );
        let lists_code_size = settings.code_size;
        let geometry = measure::measure(&lists.handles, lists_code_size, diff_width);
        let model = Model::new(settings, measure::scale(fonts.advance), geometry);
        let mut app = Self {
            focused: Focus::Area(model.area()),
            keys: keys::Keys::new(&model.settings.keymap, cx),
            driver: Driver::new(model),
            tree_focus,
            diff_focus: cx.focus_handle(),
            buffer_focus: cx.focus_handle(),
            overlay_focus: cx.focus_handle(),
            overview_scroll: ScrollHandle::new(),
            help_scroll: ScrollHandle::new(),
            inputs: Vec::new(),
            resizing: false,
            regions: Rc::new(chrome::Regions::default()),
            client_decorated: false,
            painted: PaintedRows::default(),
            fonts,
            lists,
            diff_width,
            lists_code_size,
            extent_cell: 0.0,
            laid_out: None,
            relayout: false,
            settings_width: Cell::new(0.0),
            bench: None,
            latency: Latency::default(),
            startup,
            starting: true,
            first_hunk: None,
            answering: false,
        };
        // The compositor's close button closes as Ctrl+W does, once everything is saved.
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                this.dispatch(Action::CloseWindow, window, cx);
                this.quitting == Quitting::Done
            })
            .unwrap_or(true)
        });
        let Updated { effects, .. } = app.driver.update(|model| model.open(opener));
        app.run(effects, window, cx);
        if startup.qa {
            qa::start(window, cx);
        }
        app
    }

    /// Changes the model with `change`, then runs the effects it queued and brings the gpui side
    /// in line with it: the inputs of its text fields and the focus.
    ///
    /// Every change of the model from the view goes through here, apart from the layout phase
    /// at the start of each frame.
    fn update_model<R>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Model) -> R,
    ) -> R {
        let Updated { value, effects } = self.driver.update(change);
        self.run(effects, window, cx);
        self.sync(window, cx);
        cx.notify();
        value
    }

    /// Does what `action` does, as its key would.
    fn dispatch(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        self.update_model(window, cx, |model| model.dispatch(action));
    }

    /// Runs what the model asked for: jobs on gpui's background executor with their
    /// completions handed back to the model on the UI thread, timers, the clipboard, the title,
    /// the sheets' scrolling and quitting.
    fn run(&self, effects: Vec<Effect>, window: &mut Window, cx: &mut Context<Self>) {
        for effect in effects {
            match effect {
                Effect::Spawn(job) => {
                    let work = cx.background_executor().spawn(async move { job() });
                    cx.spawn_in(window, async move |this, cx| {
                        let completion = work.await;
                        this.update_in(cx, |this, window, cx| {
                            this.update_model(window, cx, |model| model.complete(completion));
                        })
                        .ok();
                    })
                    .detach();
                }
                Effect::After(delay, completion) => {
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(delay).await;
                        this.update_in(cx, |this, window, cx| {
                            this.update_model(window, cx, |model| model.complete(completion));
                        })
                        .ok();
                    })
                    .detach();
                }
                Effect::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Effect::CopyPrimary(text) => cx.write_to_primary(ClipboardItem::new_string(text)),
                Effect::Title(title) => window.set_window_title(&title),
                Effect::ScrollSheet(step) => self.scroll_sheet(step),
                Effect::Fullscreen => window.toggle_fullscreen(),
                // QA mode quits once it has answered the command that made the app quit.
                Effect::Quit if self.answering => {}
                Effect::Quit => cx.quit(),
                Effect::Log(message) => eprintln!("review-gui: {message}"),
            }
        }
    }

    /// Brings the gpui side in line with the model after an update: an input for every open
    /// text field, the keyboard where the model gives it, and the startup work once the review
    /// opened.
    fn sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_inputs(window, cx);
        self.sync_focus(window, cx);
        if self.starting && !matches!(self.phase, Phase::Loading(_)) {
            self.starting = false;
            self.opened(window, cx);
        }
    }

    /// Keeps an input for every open text field of the model: a field opened afresh gets a new
    /// one with its text, a closed one loses its input, and text the model changed itself goes
    /// into the input.
    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self.fields();
        self.inputs.retain(|input| {
            fields
                .iter()
                .any(|field| field.field == input.field && field.id == input.id)
        });
        for field in fields {
            let FieldState {
                field: kind,
                id,
                text,
                placeholder,
                typing: _,
            } = field;
            if let Some(input) = self
                .inputs
                .iter()
                .find(|input| input.field == kind && input.id == id)
            {
                if *input.state.read(cx).value() != *text {
                    input
                        .state
                        .update(cx, |state, cx| state.set_value(text, window, cx));
                }
                continue;
            }
            let state = cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(placeholder);
                // A field reopened with its text, as the tree's filter is, continues it: the
                // caret goes at the end, where typing adds to the text.
                if !text.is_empty() {
                    state.set_value(text, window, cx);
                }
                state
            });
            let subscription = cx.subscribe_in(
                &state,
                window,
                move |this, state: &Entity<InputState>, event: &InputEvent, window, cx| {
                    this.input_event(kind, state, event, window, cx);
                },
            );
            self.inputs.push(Input {
                field: kind,
                id,
                state,
                subscription,
            });
        }
    }

    /// Hands an input's edit or Enter to the model's field `field`.
    fn input_event(
        &mut self,
        field: Field,
        state: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let text = state.read(cx).value().to_string();
                self.update_model(window, cx, |model| model.edit_field(field, &text));
            }
            InputEvent::PressEnter { .. } => {
                self.update_model(window, cx, |model| model.submit_field(field));
            }
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// The input of the open text field `field`.
    fn input(&self, field: Field) -> Option<&Entity<InputState>> {
        self.inputs
            .iter()
            .find(|input| input.field == field)
            .map(|input| &input.state)
    }

    /// Gives the keyboard to what the model gives it, when that changed since the last update.
    fn sync_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = match self.typing_field() {
            Some(field) => Focus::Field(
                field,
                self.inputs
                    .iter()
                    .find(|input| input.field == field)
                    .map_or(0, |input| input.id),
            ),
            None => Focus::Area(self.area()),
        };
        if focus == self.focused {
            return;
        }
        self.focused = focus;
        match focus {
            Focus::Field(field, _) => {
                if let Some(input) = self.input(field) {
                    input.update(cx, |input, cx| input.focus(window, cx));
                }
            }
            Focus::Area(area) => {
                let handle = match area {
                    Area::Tree => &self.tree_focus,
                    Area::Diff => &self.diff_focus,
                    Area::Buffer => &self.buffer_focus,
                    Area::Overlay
                    | Area::Picker
                    | Area::PickerList
                    | Area::SettingsList
                    | Area::SettingsSearch
                    | Area::Find
                    | Area::Window => &self.overlay_focus,
                };
                window.focus(handle, cx);
            }
        }
    }

    /// The review opened, or failed to: notes when its first hunk shows, and starts the scroll
    /// benchmark or quits when asked to.
    fn opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    }

    /// Scrolls the cheat sheet or the overview, as the model's sheet keys ask: by rows of the
    /// code's height or by half the sheet's.
    fn scroll_sheet(&self, step: SheetStep) {
        let handle = match self.overlay {
            Some(Overlay::Help) => &self.help_scroll,
            Some(Overlay::Overview) => &self.overview_scroll,
            Some(Overlay::Picker | Overlay::Settings) | None => return,
        };
        let delta = match step {
            SheetStep::Lines(lines) => scroll::narrow(f64::from(lines)) * self.sizes().row_height(),
            SheetStep::HalfPages(pages) => {
                scroll::narrow(f64::from(pages)) * f32::from(handle.bounds().size.height) / 2.0
            }
        };
        let offset = handle.offset();
        // gpui's offsets grow negative as the content scrolls up, as far as its maximum.
        let most = f32::from(handle.max_offset().y);
        let y = (f32::from(offset.y) - delta).clamp(-most, 0.0);
        handle.set_offset(gpui::point(offset.x, gpui::px(y)));
    }

    /// The window's sizes at the code size and the shown look's density.
    fn sizes(&self) -> Sizes {
        Sizes::new(&self.metrics(), self.fonts.advance)
    }

    /// Follows the diff pane's width this frame; a change of less than half a pixel is noise.
    fn follow_width(&mut self, width: f32) {
        if (width - self.diff_width).abs() >= 0.5 {
            self.diff_width = width;
        }
    }

    /// The sidebar's width as shown, zero when hidden.
    fn shown_sidebar_width(&self, window: &Window) -> f32 {
        if self.sidebar_shown() {
            review_ui::panes::sidebar_width(
                self.layout.sidebar,
                f32::from(window.viewport_size().width),
                self.sizes().ui_cell(),
            )
        } else {
            0.0
        }
    }

    fn start_bench(&mut self, mode: BenchMode, window: &mut Window, cx: &mut Context<Self>) {
        // The bench walks every file of the review, from the first.
        self.update_model(window, cx, |model| {
            if let Some(first) = file_paths(&model.tree)
                .first()
                .map(|path| (*path).to_owned())
            {
                model.show_file(&first);
            }
        });
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
        let grid = self.grid();
        let viewport = grid.viewports.diff.unwrap_or(0.0);
        let max = scroll::rows(self.rows.rows.len()) - viewport;
        let current = self.diff_scroll.offset();
        let animating = self.diff_scroll.animating();
        let row_count = self.rows.rows.len();
        let next = {
            let paths = file_paths(&self.tree);
            self.selected
                .as_deref()
                .and_then(|selected| paths.iter().position(|path| *path == selected))
                .and_then(|position| paths.get(position + 1))
                .map(|path| (*path).to_owned())
        };
        let Some(bench) = &mut self.bench else {
            return;
        };
        let now = Instant::now();
        bench.intervals.push(now - bench.last);
        bench.last = now;
        if let Some(built) = bench.built.take() {
            bench.work.push(built);
        }
        // Within half a pixel of the end.
        let end = max - 0.5 / f64::from(grid.row_height);
        if bench.intervals.len() > 1 && current >= end {
            // The end of one file: on to the next, so the bench covers the whole review.
            bench.files += 1;
            bench.rows += row_count;
            if let Some(path) = next {
                self.update_model(window, cx, |model| {
                    model.diff_scroll.stop();
                    model.show_file(&path);
                    model.diff_scroll.jump(0.0);
                });
                cx.on_next_frame(window, ReviewApp::bench_frame);
            } else {
                println!("{}", bench_report(bench));
                cx.quit();
            }
            return;
        }
        let mode = bench.mode;
        self.update_model(window, cx, |model| match mode {
            BenchMode::Fling => {
                model
                    .diff_scroll
                    .jump((current + BENCH_ROWS_PER_FRAME).min(max));
            }
            BenchMode::Animated => {
                if !animating {
                    model.ease_diff_to(current + viewport - 2.0);
                }
            }
        });
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

/// Summarizes the scroll benchmark's frame intervals and per-frame work.
fn bench_report(bench: &Bench) -> String {
    let intervals: Vec<Duration> = bench.intervals.iter().skip(1).copied().collect();
    formatdoc!(
        "
            scrolled {} rows of {} files ({:?}) in {} frames over {:.1?} ({} rows rendered)
            frame interval: {}
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
