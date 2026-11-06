//! The terminal's host of the model: terminal input in, the model's effects out, and one
//! frame after every batch of input.
//!
//! Everything that changes the model goes through [`App::update`], which runs the effects the
//! change queued: each background job on a thread of its own, so a slow language server never
//! holds up a mark's save; timers until they are due; and the clipboard and the title for the
//! terminal.
//! Each frame starts with the model's layout phase in the geometry of the panes the terminal
//! has now, then draws.
//!
//! Keys reach the model as review-gui's do: a text field taking typing gets its editing keys
//! first, as gpui-component's input binds them; everything else goes to the shared key
//! matcher with the context stack review-gui's element tree has, and keys the matcher leaves
//! go into the field.
//!
//! The mouse acts as review-gui's pointer does, on the panes of the last frame: the wheel
//! scrolls the list under it and, sideways or with Shift, pans the code; presses and drags
//! select text, fold and open tree rows, and move the files' divider; and under an overlay
//! only the overlay reacts.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Position, Rect};
use review_ui::frame::{List, Scale};
use review_ui::keys::{KeyDown, Keystroke, Matcher, Outcome, Replay, Resolution, Target};
use review_ui::line_edit::{Edited, LineEdit};
use review_ui::model::{
    Action, Completion, Driver, Effect, Field, Loaded, Mode, Model, Overlay, Phase, QuitKind,
    Settings, TextPress, Updated, list_of,
};
use review_ui::paint::display::DisplayLine;
use review_ui::paint::settings::{SettingsLine, settings_lines};
use review_ui::paint::{Palette, bars, files};
use review_ui::rows::Body;
use review_ui::text::{Column, Pane, TextPos};

use crate::color::Colors;
use crate::draw::{self, Scene, SheetScroll, first_row};
use crate::keys::{self, Focus};
use crate::log::Log;
use crate::measure::{self, FRAME, GUTTER, MARKER, Panes, SIGN, buffer_numbers, halves};

/// How many rows one turn of the wheel scrolls a list, as terminals scroll their own.
const WHEEL_ROWS: i32 = 3;

/// How many cells one sideways turn of the wheel pans the code of the diff or the buffer.
const WHEEL_CELLS: f64 = 6.0;

/// How soon a second press on the same cell counts as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// How often the loading card's dots move.
const LOADING_FRAME: Duration = Duration::from_millis(100);

/// How often the status line's failure tint fades a step.
const FLASH_FRAME: Duration = Duration::from_millis(33);

/// Something for the main loop to handle.
pub enum Message {
    /// A terminal event.
    Input(Event),
    /// A background job's completion.
    Done(Completion),
    /// A QA mode command line.
    Qa(String),
    /// The input ended or failed, which ends the app.
    InputEnded(String),
    /// A signal from outside the terminal, which the terminal's main loop handles.
    #[cfg(unix)]
    Signal(Signal),
}

/// A signal the app acts on, from outside the terminal: raw mode keeps the terminal's own keys
/// from sending any.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// A hang-up, an interrupt or a termination, by its name: the app quits once the marks are
    /// saved, which nobody may be there to see.
    End(&'static str),
    /// The app continues after a stop: it takes the terminal again and draws it whole.
    Continue,
}

/// Waits for the next messages: until `wake` when it is given, else until one comes; then
/// takes every message waiting with it, as one batch.
///
/// Returns `None` once every sender is gone.
pub fn next_batch(receiver: &Receiver<Message>, wake: Option<Instant>) -> Option<Vec<Message>> {
    let first = match wake {
        Some(at) => match receiver.recv_timeout(at.saturating_duration_since(Instant::now())) {
            Ok(message) => Some(message),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return None,
        },
        None => Some(receiver.recv().ok()?),
    };
    Some(
        first
            .into_iter()
            .chain(std::iter::from_fn(|| receiver.try_recv().ok()))
            .collect(),
    )
}

/// What the terminal itself must do for the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// Puts the text on the clipboard.
    Copy(String),
    /// Puts the text in the primary selection.
    CopyPrimary(String),
    /// Sets the terminal's title.
    Title(String),
}

/// A completion waiting for its time.
struct Timer {
    at: Instant,
    completion: Completion,
}

/// The last press of the mouse, to count a quick second one as a double click.
#[derive(Debug, Clone, Copy)]
struct Press {
    at: Instant,
    position: Position,
    count: usize,
}

/// The model as the terminal holds it.
pub struct App {
    driver: Driver,
    matcher: Matcher<Focus>,
    editor: LineEdit,
    sender: Sender<Message>,
    timers: Vec<Timer>,
    outbox: Vec<Outgoing>,
    log: Log,
    quitting: bool,
    colors: Colors,
    /// The panes of the last frame, which the mouse lands on.
    panes: Option<Panes>,
    press: Option<Press>,
    /// Where the cheat sheet or the overview is scrolled.
    sheet: SheetScroll,
    /// The overlay `sheet` belongs to, so each one opens at its top.
    sheet_of: Option<Overlay>,
    /// Whether the divider between the files and the diff is being dragged.
    resizing: bool,
}

impl App {
    /// The window over `loaded` with `settings`, on a screen `area` large, sending job
    /// completions to `sender`.
    #[must_use]
    pub fn new(
        settings: Settings,
        loaded: Loaded,
        area: Rect,
        sender: Sender<Message>,
        colors: Colors,
        log: Log,
    ) -> Self {
        let opening = Panes::lay_out(
            area,
            settings.layout,
            settings.layout.sidebar_visible,
            false,
        );
        let geometry = measure::opening(&opening, settings.code_size);
        let model = Model::new(settings, Scale::fixed(measure::sizes()), geometry);
        let matcher = Matcher::new(&model.settings.keymap);
        let mut app = Self {
            driver: Driver::new(model),
            matcher,
            editor: LineEdit::default(),
            sender,
            timers: Vec::new(),
            outbox: Vec::new(),
            log,
            quitting: false,
            colors,
            panes: None,
            press: None,
            sheet: SheetScroll::default(),
            sheet_of: None,
            resizing: false,
        };
        app.update(|model| model.open(Box::new(move || Ok(loaded))));
        app
    }

    /// The model, to read.
    #[must_use]
    pub fn model(&self) -> &Model {
        self.driver.model()
    }

    /// The panes of the last frame.
    #[must_use]
    pub fn panes(&self) -> Option<&Panes> {
        self.panes.as_ref()
    }

    /// Where the cheat sheet or the overview is scrolled.
    #[must_use]
    pub fn sheet(&self) -> SheetScroll {
        self.sheet
    }

    /// Whether the model asked to quit, which it does once everything is saved.
    #[must_use]
    pub fn quitting(&self) -> bool {
        self.quitting
    }

    /// Whether background work the model started is still running, as review-gui's QA mode
    /// waits for it.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.model().busy()
    }

    /// What the terminal must do for the model since the last call.
    pub fn take_outgoing(&mut self) -> Vec<Outgoing> {
        std::mem::take(&mut self.outbox)
    }

    /// Changes the model with `change` and runs the effects it queued.
    pub fn update<R>(&mut self, change: impl FnOnce(&mut Model) -> R) -> R {
        let Updated { value, effects } = self.driver.update(change);
        self.run(effects);
        value
    }

    /// Does what `action` does, as its key would.
    pub fn dispatch(&mut self, action: Action) {
        self.update(|model| model.dispatch(action));
    }

    /// Runs what the model asked for.
    fn run(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Spawn(job) => {
                    let sender = self.sender.clone();
                    // A thread per job, so none waits behind a slow one.
                    std::thread::spawn(move || {
                        let completion = job();
                        // The main loop is gone once the app quits; nothing waits for this.
                        sender.send(Message::Done(completion)).ok();
                    });
                }
                Effect::After(delay, completion) => self.timers.push(Timer {
                    at: Instant::now() + delay,
                    completion,
                }),
                Effect::Copy(text) => self.outbox.push(Outgoing::Copy(text)),
                Effect::CopyPrimary(text) => self.outbox.push(Outgoing::CopyPrimary(text)),
                Effect::Title(title) => self.outbox.push(Outgoing::Title(title)),
                Effect::ScrollSheet(step) => self.sheet.step(step),
                // A terminal is as full as its window; the window is the terminal's own.
                Effect::Fullscreen => {}
                Effect::Quit => self.quitting = true,
                Effect::Log(message) => self.log.write(&message),
            }
        }
    }

    /// Handles one message of the main loop.
    pub fn handle(&mut self, message: Message) {
        match message {
            Message::Input(event) => self.input(event),
            Message::Done(completion) => self.update(|model| model.complete(completion)),
            // QA lines are the QA loop's, and signals the terminal's main loop's.
            #[cfg(unix)]
            Message::Qa(_) | Message::Signal(_) => {}
            #[cfg(not(unix))]
            Message::Qa(_) => {}
            Message::InputEnded(reason) => {
                self.log
                    .write(&format!("the terminal's input ended: {reason}"));
                // Nobody can answer the app any more; it quits once the marks are saved.
                self.update(|model| model.quit(QuitKind::Unattended));
            }
        }
    }

    /// Handles a terminal event.
    pub fn input(&mut self, event: Event) {
        match event {
            Event::Key(event) => self.key(&event),
            Event::Paste(text) => self.paste(&text),
            Event::Mouse(event) => self.mouse(event),
            // The next frame lays out at the new size.
            Event::Resize(..) | Event::FocusGained | Event::FocusLost => {}
        }
    }

    /// A terminal key event.
    pub fn key(&mut self, event: &KeyEvent) {
        if let Some(key) = keys::key_down(event) {
            self.key_down(&key);
        }
    }

    /// A key press, normalized.
    fn key_down(&mut self, key: &KeyDown) {
        let now = Instant::now();
        let typing = self.model().typing_field();
        if let Some(field) = typing {
            // The field's editing keys come first, as a text input's own bindings do.
            self.follow_field();
            if let Some(edited) = self.editor.key(&key.keystroke) {
                if edited == Edited::Text {
                    self.edit_field(field);
                }
                return;
            }
        }
        let contexts = keys::contexts(self.model());
        let target = Target {
            contexts: &contexts,
            focus: Focus::of(self.model()),
            takes_text: typing.is_some(),
        };
        let Resolution { replays, outcome } = self.matcher.key_down(key, &target, now);
        self.replay(replays);
        match outcome {
            Outcome::Dispatch(action) => self.dispatch(action),
            Outcome::Pending => {}
            Outcome::Unclaimed => self.type_key(&key.keystroke),
        }
        // Enter reaches the field after the keymap, as the input reports it.
        if let Some(field) = typing
            && is_plain(&key.keystroke, "enter")
        {
            self.update(|model| model.submit_field(field));
        }
    }

    /// Does what keystrokes that waited for a sequence do once they form none.
    fn replay(&mut self, replays: Vec<Replay>) {
        for Replay { keystroke, action } in replays {
            match action {
                Some(action) => self.dispatch(action),
                None => self.type_key(&keystroke),
            }
        }
    }

    /// A keystroke no binding claims: a field taking typing edits or types it; elsewhere
    /// nothing takes it.
    fn type_key(&mut self, keystroke: &Keystroke) {
        let Some(field) = self.model().typing_field() else {
            return;
        };
        self.follow_field();
        if let Some(edited) = self.editor.key(keystroke) {
            if edited == Edited::Text {
                self.edit_field(field);
            }
            return;
        }
        let modifiers = keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        if let Some(text) = &keystroke.key_char {
            self.editor.insert(text);
            self.edit_field(field);
        }
    }

    /// A bracketed paste: text for the field taking typing; anywhere else it is dropped with a
    /// note, never typed as keys, so a pasted space never approves anything.
    pub fn paste(&mut self, text: &str) {
        match self.model().typing_field() {
            Some(field) => {
                self.follow_field();
                self.editor.insert(text);
                self.edit_field(field);
            }
            None => self.update(|model| {
                model.notice("Pasted text ignored: only a / field takes text.");
            }),
        }
    }

    /// Hands the editor's text to the model's `field`.
    fn edit_field(&mut self, field: Field) {
        let text = self.editor.text().to_owned();
        self.update(|model| model.edit_field(field, &text));
        self.follow_field();
    }

    /// Keeps the editor on the field taking typing.
    fn follow_field(&mut self) {
        let Some(field) = self.model().typing_field() else {
            return;
        };
        if let Some(state) = self
            .model()
            .fields()
            .into_iter()
            .find(|state| state.field == field)
        {
            self.editor.follow(&state);
        }
    }

    /// A mouse event, as review-gui's pointer acts: under an overlay only the overlay reacts.
    pub fn mouse(&mut self, event: MouseEvent) {
        let Some(panes) = self.panes else {
            return;
        };
        let position = Position::new(event.column, event.row);
        match self.model().overlay {
            Some(Overlay::Help | Overlay::Overview) => {
                if let Some(rows) = wheel_rows(event.kind) {
                    self.sheet.scroll_by(rows);
                }
            }
            Some(Overlay::Picker) => self.picker_mouse(&panes, event, position),
            Some(Overlay::Settings) => self.settings_mouse(&panes, event, position),
            None => self.main_mouse(&panes, event, position),
        }
    }

    /// The mouse over the files, the diff and the buffer: the wheel scrolls the list under it
    /// and pans the code of the diff and the buffer sideways, or with Shift; a press acts as
    /// review-gui's does on what is under it, and a drag selects text or moves the divider.
    fn main_mouse(&mut self, panes: &Panes, event: MouseEvent, position: Position) {
        let list = self.list_at(panes, position);
        let shift = event.modifiers.contains(KeyModifiers::SHIFT);
        let pannable = matches!(list, Some(List::Diff | List::Buffer));
        match event.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if shift && pannable => {
                let cells = if event.kind == MouseEventKind::ScrollDown {
                    WHEEL_CELLS
                } else {
                    -WHEEL_CELLS
                };
                self.pan(list, cells);
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                if let (Some(list), Some(rows)) = (list, wheel_rows(event.kind)) {
                    self.update(|model| model.wheel_list(list, f64::from(rows)));
                }
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
                let cells = if event.kind == MouseEventKind::ScrollRight {
                    WHEEL_CELLS
                } else {
                    -WHEEL_CELLS
                };
                self.pan(list, cells);
            }
            MouseEventKind::Down(MouseButton::Left) => self.press_at(panes, position, shift),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(panes, position),
            MouseEventKind::Up(MouseButton::Left) => {
                let resized = std::mem::take(&mut self.resizing);
                self.update(|model| {
                    if resized {
                        model.remember_layout();
                    }
                    model.release_text();
                });
            }
            // The button came up somewhere the terminal did not report: the drag ends.
            MouseEventKind::Moved if self.model().dragging.is_some() => {
                self.update(Model::drop_drag);
            }
            _ => {}
        }
    }

    /// Pans the code of `list` sideways by `cells`, right when positive.
    fn pan(&mut self, list: Option<List>, cells: f64) {
        match list {
            Some(List::Diff) => self.update(|model| model.pan_text(cells)),
            Some(List::Buffer) => self.update(|model| model.pan_buffer(cells)),
            _ => {}
        }
    }

    /// The mouse over the picker: the wheel scrolls its entries, a press selects one and a
    /// double press opens it.
    fn picker_mouse(&mut self, panes: &Panes, event: MouseEvent, position: Position) {
        let Some(at) = panes.picker.filter(|at| at.list.contains(position)) else {
            return;
        };
        if let Some(rows) = wheel_rows(event.kind) {
            self.update(|model| model.wheel_list(List::Picker, f64::from(rows)));
            return;
        }
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let count = self.count_press(position);
        let index = first_row(self.model(), List::Picker) + usize::from(position.y - at.list.y);
        if index < self.model().list_rows(List::Picker) {
            self.update(|model| model.click_picker_entry(index, count));
        }
    }

    /// The mouse over the settings list: the wheel scrolls it, a press on an entry applies it,
    /// one on the search gives it the keys, and one beside the panel closes it.
    fn settings_mouse(&mut self, panes: &Panes, event: MouseEvent, position: Position) {
        let Some(at) = panes.settings else {
            return;
        };
        if let Some(rows) = wheel_rows(event.kind) {
            if at.list.contains(position) {
                self.update(|model| model.wheel_list(List::Settings, f64::from(rows)));
            }
            return;
        }
        if event.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let in_panel =
            at.area.contains(position) || at.edge.is_some_and(|edge| edge.contains(position));
        if !in_panel {
            self.dispatch(Action::SettingsClose);
        } else if at.search.contains(position) {
            self.dispatch(Action::SettingsSearch);
        } else if at.list.contains(position) {
            let line =
                first_row(self.model(), List::Settings) + usize::from(position.y - at.list.y);
            let palette = Palette::of(self.model());
            let choice = match settings_lines(self.model(), &palette).into_iter().nth(line) {
                Some(SettingsLine::Entry(entry)) => Some(entry.choice),
                _ => None,
            };
            if let Some(choice) = choice {
                self.update(|model| model.click_setting(choice));
            }
        }
    }

    /// The list at `position`.
    fn list_at(&self, panes: &Panes, position: Position) -> Option<List> {
        if panes
            .sidebar
            .is_some_and(|sidebar| sidebar.tree.contains(position))
        {
            return Some(List::Tree);
        }
        panes.list.contains(position).then(|| {
            if self.buffer_shown() {
                List::Buffer
            } else {
                List::Diff
            }
        })
    }

    /// Whether the file buffer shows rather than the diff.
    fn buffer_shown(&self) -> bool {
        self.model().mode == Mode::Buffer && self.model().buffer.is_some()
    }

    /// Counts a press at `position` as a quick series of presses on the same cell, and
    /// returns which press of the series it is.
    fn count_press(&mut self, position: Position) -> usize {
        let now = Instant::now();
        let count = match self.press {
            Some(last) if last.position == position && now - last.at < DOUBLE_CLICK => {
                last.count + 1
            }
            _ => 1,
        };
        self.press = Some(Press {
            at: now,
            position,
            count,
        });
        count
    }

    /// A press of the left button at `position`, extending a selection when `shift`: on the
    /// divider it starts a drag or, double, resets the files' width; on a tree row, its click;
    /// on the diff or the buffer, the text there.
    fn press_at(&mut self, panes: &Panes, position: Position, shift: bool) {
        let count = self.count_press(position);
        if panes
            .divider
            .is_some_and(|divider| divider.contains(position))
        {
            if count >= 2 {
                self.resizing = false;
                self.update(Model::reset_sidebar_width);
            } else {
                self.resizing = true;
            }
            return;
        }
        if let Some(sidebar) = panes
            .sidebar
            .filter(|sidebar| sidebar.tree.contains(position))
        {
            let row = usize::from(position.y - sidebar.tree.y);
            let tree_row = first_row(self.model(), List::Tree) + row;
            let palette = Palette::of(self.model());
            let Some(node) = files::tree_row(self.model(), &palette, tree_row) else {
                return;
            };
            self.update(|model| model.click_tree_row(tree_row, node.index, count));
            return;
        }
        if !panes.list.contains(position) {
            return;
        }
        let pane = if self.buffer_shown() {
            Pane::Source
        } else {
            Pane::Diff
        };
        // A shift-press extends a selection in its own column; anything else starts in the
        // column under the pointer.
        let column = match self.model().selection {
            Some(selection) if shift && selection.pane == pane => selection.column,
            _ => self.column_at(panes, pane, position.x),
        };
        let Some(pos) = self.text_pos(panes, pane, column, position) else {
            return;
        };
        let press = TextPress {
            pane,
            column,
            pos,
            clicks: count,
            extend: shift,
        };
        self.update(|model| model.press_text(press));
    }

    /// The pointer dragged to `position` with the button down: the divider follows it, or the
    /// selection extends there, scrolling a row at a time past the list's edges.
    fn drag_to(&mut self, panes: &Panes, position: Position) {
        if self.resizing {
            let width = review_ui::panes::sidebar_cells(f32::from(position.x), panes.area.width);
            self.update(|model| model.set_sidebar_width(f32::from(width)));
            return;
        }
        let Some(pane) = self.model().dragging else {
            return;
        };
        let list = list_of(pane);
        let area = panes.list;
        let overshoot = if position.y < area.top() {
            -1.0
        } else if position.y >= area.bottom() {
            1.0
        } else {
            0.0
        };
        if overshoot != 0.0 {
            self.update(|model| model.scroll_list_by(list, overshoot));
        }
        let column = self
            .model()
            .selection
            .map_or(Column::Whole, |selection| selection.column);
        if let Some(pos) = self.text_pos(panes, pane, column, position) {
            self.update(|model| model.drag_text(pane, pos));
        }
    }

    /// The column of the diff at `x`: a half of the side-by-side diff, or the whole width.
    fn column_at(&self, panes: &Panes, pane: Pane, x: u16) -> Column {
        if pane != Pane::Diff || !self.model().side_by_side() {
            return Column::Whole;
        }
        let (left, _) = halves(panes.list.width);
        if x < panes.list.x + FRAME + left {
            Column::Left
        } else {
            Column::Right
        }
    }

    /// The place in `pane`'s text at `position`, reading a pair's line from `column`: above
    /// the list, the start of its first row; below it, the end of its last.
    fn text_pos(
        &self,
        panes: &Panes,
        pane: Pane,
        column: Column,
        position: Position,
    ) -> Option<TextPos> {
        let model = self.model();
        let list = list_of(pane);
        let area = panes.list;
        let last_row = model.list_rows(list).checked_sub(1)?;
        let first = first_row(model, list);
        let shown = |offset: u16| (first + usize::from(offset)).min(last_row);
        let (row, edge) = if position.y < area.top() {
            (shown(0), Some(false))
        } else if position.y >= area.bottom() {
            (shown(area.height.saturating_sub(1)), Some(true))
        } else {
            (shown(position.y - area.top()), None)
        };
        let len = model.copyable(pane, column, row).map_or(0, str::len);
        let col = match edge {
            Some(false) => 0,
            Some(true) => len,
            None => self
                .byte_at(panes, pane, column, row, position.x)
                .unwrap_or(len),
        };
        Some(TextPos {
            row,
            col: col.min(len),
        })
    }

    /// The byte of `row`'s text in `pane` drawn at column `x`, reading a pair's line from
    /// `column`; `None` on a row without code, such as one of the tool's own.
    fn byte_at(
        &self,
        panes: &Panes,
        pane: Pane,
        column: Column,
        row: usize,
        x: u16,
    ) -> Option<usize> {
        let model = self.model();
        let area = panes.list;
        let (text, code_at, skip) = match pane {
            Pane::Source => {
                let buffer = model.buffer.as_ref()?;
                let numbers = buffer_numbers(buffer.lines.lines.len());
                let skip = review_ui::scroll::whole_rows(model.buffer_text.offset());
                (buffer.line(row), area.x + MARKER + numbers, skip)
            }
            Pane::Diff => {
                let skip = review_ui::scroll::whole_rows(model.diff_text().offset());
                let (left, _) = halves(area.width);
                match &model.rows.rows.get(row)?.body {
                    Body::Code(cell) => (cell.text.as_str(), area.x + FRAME + GUTTER + SIGN, skip),
                    Body::Pair(pair) => {
                        let (cell, half_at) = match column {
                            Column::Left => (pair.left.as_ref(), area.x + FRAME),
                            Column::Right => (
                                pair.right.as_ref(),
                                area.x + FRAME + left + crate::measure::DIVIDER,
                            ),
                            Column::Whole => (None, area.x),
                        };
                        (cell?.text.as_str(), half_at + GUTTER + SIGN, skip)
                    }
                    Body::Note(_) => return None,
                }
            }
        };
        let cell = usize::from(x.saturating_sub(code_at)) + skip;
        Some(DisplayLine::new(text).byte_at(cell))
    }

    /// Runs the timers due at `now`, oldest first, and replays keys whose sequence timed out.
    pub fn tick(&mut self, now: Instant) {
        loop {
            let due = self
                .timers
                .iter()
                .enumerate()
                .filter(|(_, timer)| timer.at <= now)
                .min_by_key(|(_, timer)| timer.at)
                .map(|(index, _)| index);
            let Some(index) = due else {
                break;
            };
            let Timer { completion, .. } = self.timers.remove(index);
            self.update(|model| model.complete(completion));
        }
        if self
            .matcher
            .deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            let contexts = keys::contexts(self.model());
            let target = Target {
                contexts: &contexts,
                focus: Focus::of(self.model()),
                takes_text: self.model().typing_field().is_some(),
            };
            let replays = self.matcher.timeout(&target, now);
            self.replay(replays);
        }
    }

    /// When the main loop must wake by itself: a timer, the key matcher's deadline, or the
    /// next step of something animating on screen; `None` waits for input.
    #[must_use]
    pub fn wake_at(&self, now: Instant) -> Option<Instant> {
        let animation = if matches!(self.model().phase, Phase::Loading(_)) {
            Some(now + LOADING_FRAME)
        } else if bars::flashing(self.model(), now) {
            Some(now + FLASH_FRAME)
        } else {
            None
        };
        self.timers
            .iter()
            .map(|timer| timer.at)
            .chain(self.matcher.deadline())
            .chain(animation)
            .min()
    }

    /// Lays out and draws one frame on `terminal`: the panes at its size, the model's layout
    /// phase against them, then every pane.
    ///
    /// # Errors
    ///
    /// Returns the terminal's error when it cannot be measured or drawn to.
    pub fn render<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<(), B::Error> {
        let size = terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        self.follow_field();
        let panes = Panes::new(area, self.model());
        let geometry = measure::geometry(&panes, self.model());
        let now = Instant::now();
        self.update(|model| model.layout(geometry, now));
        self.panes = Some(panes);
        // Each sheet opens at its top.
        let overlay = self.model().overlay;
        if overlay != self.sheet_of {
            self.sheet_of = overlay;
            self.sheet = SheetScroll::default();
        }
        let Self {
            driver,
            editor,
            sheet,
            colors,
            ..
        } = self;
        let model = driver.model();
        terminal.draw(|frame| {
            let scene = Scene {
                panes: &panes,
                editor,
                sheet,
            };
            draw::frame(frame.buffer_mut(), *colors, model, scene, now);
        })?;
        Ok(())
    }

    /// Stops what the app started before it goes: the language servers that no lookup uses.
    pub fn shut_down(&mut self) {
        self.update(Model::stop_language_servers);
    }
}

/// The rows a turn of the wheel scrolls, down when positive; `None` for anything but the
/// wheel turned up or down.
fn wheel_rows(kind: MouseEventKind) -> Option<i32> {
    match kind {
        MouseEventKind::ScrollDown => Some(WHEEL_ROWS),
        MouseEventKind::ScrollUp => Some(-WHEEL_ROWS),
        _ => None,
    }
}

/// Whether `keystroke` is `key` without modifiers.
fn is_plain(keystroke: &Keystroke, key: &str) -> bool {
    keystroke.key == key && keystroke.modifiers == review_ui::keys::Modifiers::default()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::backend::TestBackend;
    use review_core::demo::{DemoPaths, DemoRequest, fixture_dir, prepare};
    use review_ui::front_end::FrontEnd;
    use review_ui::launch::{self, Configured};

    use super::*;

    /// The app over a fresh demo review in `dir`, on a 160×50 screen, and where its jobs
    /// report.
    fn demo(dir: &std::path::Path) -> (App, Receiver<Message>) {
        let review = prepare(&DemoPaths::under(dir), &fixture_dir(), DemoRequest::Fresh).unwrap();
        let history = review.history().unwrap();
        let settings = launch::settings(
            Configured {
                config: review_ui::config::Config::default(),
                path: None,
                problems: Vec::new(),
            },
            FrontEnd::Terminal,
        )
        .unwrap();
        let (sender, receiver) = mpsc::channel();
        let app = App::new(
            settings,
            Loaded {
                review,
                history,
                found: None,
            },
            Rect::new(0, 0, 160, 50),
            sender,
            Colors::TrueColor,
            Log::direct(),
        );
        (app, receiver)
    }

    /// Draws frames and runs completions and timers until no background work is left.
    fn settle(app: &mut App, receiver: &Receiver<Message>, terminal: &mut Terminal<TestBackend>) {
        loop {
            app.render(terminal).unwrap();
            app.tick(Instant::now());
            if !app.busy() {
                app.render(terminal).unwrap();
                return;
            }
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(message) => app.handle(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => panic!("the jobs' channel closed"),
            }
        }
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.input(Event::Key(KeyEvent::new(code, modifiers)));
    }

    /// A paste in the files is never typed as keys: a pasted space approves nothing and the
    /// status line says the paste was dropped, while a typed space approves.
    #[test]
    fn pastes_never_act_as_keys() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        settle(&mut app, &receiver, &mut terminal);
        let before = app.model().totals();

        app.paste(" ");
        settle(&mut app, &receiver, &mut terminal);
        assert_eq!(app.model().totals(), before);
        assert!(app.model().status.starts_with("Pasted text ignored"));

        press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        settle(&mut app, &receiver, &mut terminal);
        assert_ne!(app.model().totals(), before, "a typed space approves");
    }

    /// The screen's rows as text, wide symbols once.
    fn screen(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
                    .collect()
            })
            .collect()
    }

    /// A note wider than the diff ends in the clip marker, and once the code pans as far as it
    /// goes, its end shows: a symlink's entry is never cut off for good.
    #[test]
    fn a_long_note_pans_to_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(70, 20)).unwrap();
        settle(&mut app, &receiver, &mut terminal);
        app.update(|model| model.show_file("docs/current"));
        app.dispatch(Action::FocusDiff);
        settle(&mut app, &receiver, &mut terminal);
        let row = screen(&terminal)
            .iter()
            .position(|row| row.contains("before  docs/"))
            .unwrap();
        let cut = screen(&terminal).swap_remove(row);
        assert!(cut.trim_end().ends_with('…'), "{cut:?}");
        assert!(!cut.contains("v1.md"), "{cut:?}");

        app.update(|model| model.pan_text(1000.0));
        settle(&mut app, &receiver, &mut terminal);
        let panned = screen(&terminal).swap_remove(row);
        assert!(panned.trim_end().ends_with("\"v1.md\""), "{panned:?}");
    }

    /// A terminal too narrow to give the file finder's query any cells still draws it, its
    /// cursor and all, rather than panicking on the room it lacks.
    #[test]
    fn a_field_without_room_draws_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
        settle(&mut app, &receiver, &mut terminal);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        for character in "eng".chars() {
            press(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
        }
        settle(&mut app, &receiver, &mut terminal);
        assert_eq!(app.model().overlay, Some(review_ui::model::Overlay::Picker));
    }

    /// The `/` filter takes typed keys, pastes and the editing keys, Ctrl+W among them, which
    /// a terminal has no window to close with; Enter keeps the filter and gives the tree its
    /// keys back without opening anything.
    #[test]
    fn the_filter_takes_typing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        settle(&mut app, &receiver, &mut terminal);

        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        for character in "en q".chars() {
            press(&mut app, KeyCode::Char(character), KeyModifiers::NONE);
        }
        let query = |app: &App| {
            app.model()
                .filter
                .as_ref()
                .map(|filter| filter.query.clone())
        };
        assert_eq!(
            query(&app).as_deref(),
            Some("en q"),
            "q types rather than quits"
        );
        assert!(!app.quitting());
        press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(query(&app).as_deref(), Some("en "));
        press(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        app.paste("gine");
        assert_eq!(query(&app).as_deref(), Some("engine"));

        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        settle(&mut app, &receiver, &mut terminal);
        assert_eq!(app.model().mode, Mode::Tree);
        assert!(
            app.model()
                .filter
                .as_ref()
                .is_some_and(|filter| !filter.editing)
        );
        assert_eq!(app.model().selected.as_deref(), Some("src/engine.rs"));
    }

    /// Escape in an overlay does what it does in review-gui: the picker's first Escape gives
    /// its list the keys and the second closes it, while the cheat sheet and the settings list
    /// close at once.
    #[test]
    fn escape_leaves_overlays_as_review_gui_does() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        settle(&mut app, &receiver, &mut terminal);
        app.dispatch(Action::FindFiles);
        settle(&mut app, &receiver, &mut terminal);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        settle(&mut app, &receiver, &mut terminal);
        assert_eq!(app.model().overlay, Some(Overlay::Picker));
        assert!(
            app.model()
                .picker
                .as_ref()
                .is_some_and(|picker| !picker.editing)
        );
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        settle(&mut app, &receiver, &mut terminal);
        assert_eq!(app.model().overlay, None);
        for open in [
            Action::ToggleHelp,
            Action::OpenSettings,
            Action::ToggleOverview,
        ] {
            app.dispatch(open);
            settle(&mut app, &receiver, &mut terminal);
            assert!(app.model().overlay.is_some(), "{open:?} opens an overlay");
            press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            settle(&mut app, &receiver, &mut terminal);
            assert_eq!(
                app.model().overlay,
                None,
                "Escape closes what {open:?} opened"
            );
        }
    }

    /// The left button at `column`, `row`, with `modifiers`.
    fn mouse_at(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// A drag over code panned sideways selects the characters drawn under the pointer, which
    /// lie past the line's start by the pan, and the release puts them in the primary
    /// selection as review-gui's does.
    #[test]
    fn a_drag_over_panned_code_selects_what_shows() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        // A screen narrow enough that the file's longest lines run past the diff's edge.
        let mut terminal = Terminal::new(TestBackend::new(100, 50)).unwrap();
        settle(&mut app, &receiver, &mut terminal);
        app.update(|model| model.show_file("src/engine.rs"));
        app.dispatch(Action::FocusDiff);
        settle(&mut app, &receiver, &mut terminal);
        app.dispatch(Action::PanRight);
        settle(&mut app, &receiver, &mut terminal);
        let skip = review_ui::scroll::whole_rows(app.model().diff_text().offset());
        assert!(skip > 0, "the pan moved the code");

        // The first row on screen whose code reaches past the pan by a word or more.
        let panes = app.panes.unwrap();
        let first = first_row(app.model(), List::Diff);
        let (offset, text) = (0..panes.list.height)
            .find_map(|offset| {
                let row = app.model().rows.rows.get(first + usize::from(offset))?;
                match &row.body {
                    Body::Code(cell) if cell.text.is_ascii() && cell.text.len() >= skip + 6 => {
                        Some((offset, cell.text.clone()))
                    }
                    _ => None,
                }
            })
            .unwrap();
        let y = panes.list.y + offset;
        let code_at = panes.list.x + FRAME + GUTTER + SIGN;
        let _ = app.take_outgoing();

        mouse_at(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            code_at,
            y,
        );
        mouse_at(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            code_at + 3,
            y,
        );
        mouse_at(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            code_at + 5,
            y,
        );
        mouse_at(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            code_at + 5,
            y,
        );
        settle(&mut app, &receiver, &mut terminal);
        assert!(
            app.take_outgoing()
                .contains(&Outgoing::CopyPrimary(text[skip..skip + 5].to_owned())),
            "the selection is the five characters shown from the pan on: {:?}",
            &text[skip..skip + 5]
        );
    }

    /// Dragging the divider sets the files' width, and a double press on it resets it.
    #[test]
    fn the_divider_drags_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        settle(&mut app, &receiver, &mut terminal);
        let divider = app.panes.unwrap().divider.unwrap();
        let width = |app: &App| app.panes.unwrap().list.x;
        let before = width(&app);

        mouse_at(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            divider.x,
            divider.y,
        );
        mouse_at(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            40,
            divider.y,
        );
        mouse_at(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            40,
            divider.y,
        );
        settle(&mut app, &receiver, &mut terminal);
        assert_eq!(
            width(&app),
            41,
            "the diff starts right of the moved divider"
        );

        let divider = app.panes.unwrap().divider.unwrap();
        for _ in 0..2 {
            mouse_at(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                divider.x,
                divider.y,
            );
            mouse_at(
                &mut app,
                MouseEventKind::Up(MouseButton::Left),
                divider.x,
                divider.y,
            );
        }
        settle(&mut app, &receiver, &mut terminal);
        assert_eq!(width(&app), before, "a double press resets the width");
    }

    /// Keys the terminal cannot honor do nothing: zoom keeps the code size.
    #[test]
    fn window_keys_do_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (mut app, receiver) = demo(dir.path());
        let mut terminal = Terminal::new(TestBackend::new(160, 50)).unwrap();
        settle(&mut app, &receiver, &mut terminal);
        let size = app.model().code_size;
        press(&mut app, KeyCode::Char('='), KeyModifiers::CONTROL);
        app.dispatch(Action::ZoomIn);
        assert_eq!(app.model().code_size.to_bits(), size.to_bits());
        assert!(!app.quitting());
    }
}
