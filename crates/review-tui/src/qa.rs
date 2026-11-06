//! QA mode: the line commands of review-gui's QA mode on standard input, answered on
//! standard output, with the screen drawn into an off-screen buffer instead of a terminal.
//!
//! Commands, one per line, each answered with one line (`ok`, `error: ...`, or the answer)
//! once the background work it started is done and a frame has been drawn after it:
//!
//! - `key <keystrokes>` types keystrokes as a terminal sends them, through the same
//!   normalization real keys take, for example `key j`, `key shift-s` or `key z a`.
//! - `action <name>` does what one of the model's actions does, as its key would.
//! - `paste <json string>` pastes text as a terminal's bracketed paste does, such as
//!   `paste " "`.
//! - `click <column> <row> [count] [shift]` presses the left button on a cell, `count` times
//!   in a row, with Shift held when `shift` is given.
//! - `drag <column> <row> <column> <row>` presses the left button on the first cell, moves to
//!   the second a cell at a time, and releases it there.
//! - `wheel <turns> [<column> <row>] [shift]` turns the wheel, down when positive, over the
//!   diff or at a cell, with Shift held when `shift` is given.
//! - `scroll <rows>` scrolls the diff by rows, down when positive.
//! - `pan <cells>` moves the diff's code left by cells.
//! - `file <path>` shows a file's diff and puts the tree cursor on it.
//! - `find <text>` selects the hunk of the next diff row containing `text`.
//! - `resize <columns>x<rows>` resizes the screen, 160x50 at first.
//! - `shot <path>` writes the screen to `<path>.txt` as text and to `<path>.ansi` with its
//!   colors as SGR sequences.
//! - `clipboard` prints what the app last copied to the clipboard, as a JSON string, and
//!   `primary` what it last put in the primary selection.
//! - `state` prints the observable state as one line of JSON, as review-gui's does, with the
//!   geometry in cells.
//! - `quit` quits as `q` does: once the marks and folded folders still being saved are saved.
//! - `rapid <command>` answers without waiting for the work the command starts;
//!   `rapid state` prints the state at once.

use std::collections::VecDeque;
use std::io::{BufRead as _, Write as _};
use std::ops::Range;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::time::Instant;

use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth as _};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use review_ui::frame::List;
use review_ui::keys::Keystroke;
use review_ui::model::{Action, Loaded, Model, QuitKind, Quitting, Settings};
use review_ui::state;
use serde_json::{Map, Value, json};

use crate::app::{App, Message, Outgoing, next_batch};
use crate::color::Colors;
use crate::draw::{SheetScroll, first_row};
use crate::keys;
use crate::log::Log;
use crate::measure::Panes;

/// The screen QA mode starts with, in cells.
const SIZE: (u16, u16) = (160, 50);

/// A parsed QA command.
#[derive(Debug, Clone, PartialEq)]
enum Command {
    Key(Vec<String>),
    Action(String),
    Paste(String),
    Click {
        column: u16,
        row: u16,
        count: usize,
        shift: bool,
    },
    Drag([u16; 4]),
    Wheel {
        turns: i32,
        at: Option<(u16, u16)>,
        shift: bool,
    },
    Scroll(f64),
    Pan(f64),
    File(String),
    Find(String),
    Resize {
        columns: u16,
        rows: u16,
    },
    Shot(String),
    Clipboard,
    Primary,
    State,
    Quit,
}

/// The `what` in `word`, parsed.
fn number<T: std::str::FromStr>(word: Option<&str>, what: &str) -> Result<T, String> {
    word.and_then(|word| word.parse().ok())
        .ok_or_else(|| format!("expected {what}"))
}

impl Command {
    fn parse(line: &str) -> Result<Self, String> {
        let (verb, argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = argument.trim();
        // A trailing `shift` holds Shift for a click or a turn of the wheel.
        let (argument, shift) = match argument.strip_suffix("shift") {
            Some(rest) if matches!(verb, "click" | "wheel") => (rest.trim_end(), true),
            _ => (argument, false),
        };
        let mut words = argument.split_whitespace();
        match verb {
            "key" if !argument.is_empty() => Ok(Self::Key(
                argument.split_whitespace().map(str::to_owned).collect(),
            )),
            "action" if !argument.is_empty() => Ok(Self::Action(argument.to_owned())),
            "paste" => serde_json::from_str(argument)
                .map(Self::Paste)
                .map_err(|err| format!("paste takes a JSON string: {err}")),
            "click" => Ok(Self::Click {
                column: number(words.next(), "a column")?,
                row: number(words.next(), "a row")?,
                count: words
                    .next()
                    .map_or(Ok(1), |count| number(Some(count), "a count"))?,
                shift,
            }),
            "drag" => Ok(Self::Drag([
                number(words.next(), "a column")?,
                number(words.next(), "a row")?,
                number(words.next(), "a column")?,
                number(words.next(), "a row")?,
            ])),
            "wheel" => {
                let turns = number(words.next(), "a number of turns")?;
                let at = match words.next() {
                    Some(column) => Some((
                        number(Some(column), "a column")?,
                        number(words.next(), "a row")?,
                    )),
                    None => None,
                };
                Ok(Self::Wheel { turns, at, shift })
            }
            "scroll" => Ok(Self::Scroll(number(words.next(), "a number of rows")?)),
            "pan" => Ok(Self::Pan(number(words.next(), "a number of cells")?)),
            "file" if !argument.is_empty() => Ok(Self::File(argument.to_owned())),
            "find" if !argument.is_empty() => Ok(Self::Find(argument.to_owned())),
            "resize" => {
                let (columns, rows) = argument
                    .split_once('x')
                    .and_then(|(columns, rows)| Some((columns.parse().ok()?, rows.parse().ok()?)))
                    .ok_or("resize takes <columns>x<rows>")?;
                Ok(Self::Resize { columns, rows })
            }
            "shot" if !argument.is_empty() => Ok(Self::Shot(argument.to_owned())),
            "clipboard" => Ok(Self::Clipboard),
            "primary" => Ok(Self::Primary),
            "state" => Ok(Self::State),
            "quit" => Ok(Self::Quit),
            _ => Err(format!("unknown command {line:?}")),
        }
    }
}

/// The QA session: the app on an off-screen terminal, and what it copied.
struct Session {
    app: App,
    terminal: Terminal<TestBackend>,
    clipboard: Option<String>,
    primary: Option<String>,
}

/// Reads command lines from standard input on a thread of its own, sending each to `sender`
/// and then the end of the input.
fn read_commands(sender: Sender<Message>) {
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if sender.send(Message::Qa(line)).is_err() {
                return;
            }
        }
        // The main loop is gone once the session quit; nothing waits for this.
        sender
            .send(Message::InputEnded("standard input ended".to_owned()))
            .ok();
    });
}

/// Runs QA mode over `loaded` until the app quits, by `quit`, by its keys or at the end of
/// standard input, each as the reviewer's `q` quits it.
///
/// # Errors
///
/// Returns an error when the off-screen terminal cannot be made.
pub fn run(settings: Settings, loaded: Loaded) -> color_eyre::eyre::Result<()> {
    let (sender, receiver) = mpsc::channel();
    read_commands(sender.clone());
    let (columns, rows) = SIZE;
    let terminal = Terminal::new(TestBackend::new(columns, rows))?;
    let app = App::new(
        settings,
        loaded,
        Rect::new(0, 0, columns, rows),
        sender,
        Colors::TrueColor,
        Log::direct(),
    );
    let mut session = Session {
        app,
        terminal,
        clipboard: None,
        primary: None,
    };
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut waiting: Option<Command> = None;
    // Whether standard input ended, after which the queued commands still run.
    let mut ended = false;
    loop {
        session.render();
        // A command whose work settled answers now that a frame shows it.
        if let Some(command) = waiting.take_if(|_| !session.app.busy()) {
            answer(&session.reply(&command));
        }
        while waiting.is_none()
            && let Some(line) = queue.pop_front()
        {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (rapid, line) = match line.strip_prefix("rapid ") {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            match Command::parse(line).and_then(|command| session.start(&command).map(|()| command))
            {
                Err(err) => answer(&format!("error: {err}")),
                Ok(Command::State) if rapid => answer(&session.state().to_string()),
                Ok(_) if rapid => answer("ok"),
                Ok(command) => waiting = Some(command),
            }
        }
        // The model quits once what the reviewer did is saved; a command still waiting quit it.
        if session.app.quitting() {
            if waiting.is_some() {
                answer("ok");
            }
            break;
        }
        // The end of the input quits once what the reviewer did is saved, which nobody is
        // left to see.
        if ended
            && queue.is_empty()
            && waiting.is_none()
            && matches!(
                session.app.model().quitting,
                Quitting::No | Quitting::Stopped
            )
        {
            session.app.update(|model| model.quit(QuitKind::Unattended));
            continue;
        }
        // A command waiting for a frame and no work needs no message to go on.
        let wake = if waiting.is_some() && !session.app.busy() {
            Some(Instant::now())
        } else {
            session.app.wake_at(Instant::now())
        };
        let Some(batch) = next_batch(&receiver, wake) else {
            break;
        };
        for message in batch {
            match message {
                Message::Qa(line) => queue.push_back(line),
                Message::InputEnded(_) => ended = true,
                other => session.app.handle(other),
            }
        }
        session.app.tick(Instant::now());
    }
    session.app.shut_down();
    Ok(())
}

/// Prints one answer line.
fn answer(text: &str) {
    let mut stdout = std::io::stdout().lock();
    // A closed standard output leaves no one to answer.
    writeln!(stdout, "{text}")
        .and_then(|()| stdout.flush())
        .ok();
}

impl Session {
    /// Draws a frame and keeps what the app copied.
    fn render(&mut self) {
        match self.app.render(&mut self.terminal) {
            Ok(()) => {}
            Err(never) => match never {},
        }
        for outgoing in self.app.take_outgoing() {
            match outgoing {
                Outgoing::Copy(text) => self.clipboard = Some(text),
                Outgoing::CopyPrimary(text) => self.primary = Some(text),
                // Off screen there is no title to set.
                Outgoing::Title(_) => {}
            }
        }
    }

    /// Starts `command`.
    fn start(&mut self, command: &Command) -> Result<(), String> {
        match command {
            Command::Key(keystrokes) => {
                for source in keystrokes {
                    let keystroke = Keystroke::parse(source).map_err(|err| err.to_string())?;
                    let event = keys::key_event(&keystroke)
                        .ok_or_else(|| format!("a terminal cannot send {source}"))?;
                    self.app.input(Event::Key(event));
                }
            }
            Command::Action(name) => {
                let action =
                    Action::from_name(name).ok_or_else(|| format!("unknown action {name:?}"))?;
                self.app.dispatch(action);
            }
            Command::Paste(text) => self.app.input(Event::Paste(text.clone())),
            Command::Click {
                column,
                row,
                count,
                shift,
            } => {
                let held = held(*shift);
                for _ in 0..*count {
                    let down = MouseEventKind::Down(MouseButton::Left);
                    self.app.input(mouse(down, *column, *row, held));
                    let up = MouseEventKind::Up(MouseButton::Left);
                    self.app.input(mouse(up, *column, *row, held));
                }
            }
            Command::Drag([from_column, from_row, to_column, to_row]) => {
                let none = KeyModifiers::NONE;
                let down = MouseEventKind::Down(MouseButton::Left);
                self.app.input(mouse(down, *from_column, *from_row, none));
                self.render();
                // A cell at a time, as a terminal reports a drag.
                let steps = from_column
                    .abs_diff(*to_column)
                    .max(from_row.abs_diff(*to_row))
                    .max(1);
                for step in 1..=steps {
                    let along = |from: u16, to: u16| {
                        let from = i32::from(from);
                        let span = i32::from(to) - from;
                        let at = from + span * i32::from(step) / i32::from(steps);
                        u16::try_from(at).unwrap_or(to)
                    };
                    let drag = MouseEventKind::Drag(MouseButton::Left);
                    let (column, row) =
                        (along(*from_column, *to_column), along(*from_row, *to_row));
                    self.app.input(mouse(drag, column, row, none));
                }
                let up = MouseEventKind::Up(MouseButton::Left);
                self.app.input(mouse(up, *to_column, *to_row, none));
            }
            Command::Wheel { turns, at, shift } => {
                // Over the middle of the diff unless a cell is named.
                let list = self.app.panes().map_or(Rect::default(), |panes| panes.list);
                let (column, row) =
                    at.unwrap_or((list.x + list.width / 2, list.y + list.height / 2));
                let kind = if *turns >= 0 {
                    MouseEventKind::ScrollDown
                } else {
                    MouseEventKind::ScrollUp
                };
                for _ in 0..turns.unsigned_abs() {
                    self.app.input(mouse(kind, column, row, held(*shift)));
                }
            }
            Command::Scroll(rows) => self
                .app
                .update(|model| model.scroll_list_by(List::Diff, *rows)),
            Command::Pan(cells) => self.app.update(|model| model.pan_text(*cells)),
            Command::File(path) => {
                if !state::is_file(&self.app.model().tree, path) {
                    return Err(format!("no file {path}"));
                }
                self.app.update(|model| model.show_file(path));
            }
            Command::Find(text) => self
                .app
                .update(|model| model.select_hunk_containing(text))?,
            Command::Resize { columns, rows } => {
                self.terminal.backend_mut().resize(*columns, *rows);
            }
            Command::Shot(path) => {
                // The shot shows the frame after everything before it.
                self.render();
                shot(self.terminal.backend().buffer(), Path::new(path))
                    .map_err(|err| format!("cannot write the shot {path}: {err}"))?;
            }
            Command::Quit => self.app.dispatch(Action::Quit),
            Command::Clipboard | Command::Primary | Command::State => {}
        }
        Ok(())
    }

    /// The answer of a settled `command`.
    fn reply(&self, command: &Command) -> String {
        match command {
            Command::State => self.state().to_string(),
            Command::Clipboard => json!(self.clipboard).to_string(),
            Command::Primary => json!(self.primary).to_string(),
            _ => "ok".to_owned(),
        }
    }

    /// The observable state, in cells.
    fn state(&self) -> Value {
        let model = self.app.model();
        let panes = self.app.panes().copied();
        let screen = CellScreen {
            model,
            panes,
            diff_width: panes.map_or(0, |panes| panes.main.width),
            sheet: self.app.sheet(),
        };
        review_ui::qa::state(model, &screen)
    }
}

/// A terminal mouse event of `kind` at a cell with `modifiers` held.
fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

/// Shift held when `shift`, else nothing.
fn held(shift: bool) -> KeyModifiers {
    if shift {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    }
}

/// Where things are on the terminal, in cells, for QA mode's `state`.
struct CellScreen<'a> {
    model: &'a Model,
    panes: Option<Panes>,
    diff_width: u16,
    /// Where the cheat sheet or the overview is scrolled.
    sheet: SheetScroll,
}

impl CellScreen<'_> {
    /// The diff list's area.
    fn list(&self) -> Rect {
        self.panes.map_or(Rect::default(), |panes| panes.list)
    }
}

impl review_ui::qa::Screen for CellScreen<'_> {
    fn rows(&self) -> Range<usize> {
        let first = first_row(self.model, List::Diff);
        let end = (first + usize::from(self.list().height)).min(self.model.rows.rows.len());
        first.min(end)..end
    }

    fn row_y(&self, row: usize) -> Value {
        let first = first_row(self.model, List::Diff);
        json!(usize::from(self.list().y) + row.saturating_sub(first))
    }

    fn tree_y(&self, position: usize) -> Value {
        let top = self
            .panes
            .and_then(|panes| panes.sidebar)
            .map_or(0, |sidebar| i64::from(sidebar.tree.y));
        let first = first_row(self.model, List::Tree);
        let offset =
            i64::try_from(position).unwrap_or(i64::MAX) - i64::try_from(first).unwrap_or(0);
        json!(top + offset)
    }

    fn list(&self) -> Map<String, Value> {
        let rows = self.rows();
        let list = CellScreen::list(self);
        let offset = self
            .model
            .list_scroll(List::Diff)
            .map_or(0.0, review_ui::frame::ListScroll::offset);
        let mut entries = Map::new();
        entries.insert("visible_rows".to_owned(), json!([rows.start, rows.end]));
        entries.insert("scroll_offset".to_owned(), json!(offset));
        entries.insert("row_height".to_owned(), json!(1));
        entries.insert(
            "viewport".to_owned(),
            json!([list.x, list.y, list.width, list.height]),
        );
        entries
    }

    fn window(&self) -> Map<String, Value> {
        let sidebar = self
            .panes
            .and_then(|panes| panes.sidebar)
            .map(|sidebar| sidebar.area.width);
        let mut entries = Map::new();
        entries.insert("layout".to_owned(), json!({ "sidebar": sidebar }));
        let SheetScroll {
            offset,
            max,
            viewport: _,
        } = self.sheet;
        entries.insert("sheet".to_owned(), json!({ "offset": offset, "max": max }));
        entries
    }

    fn settings(&self) -> Value {
        let Some(settings) = self.panes.and_then(|panes| panes.settings) else {
            return Value::Null;
        };
        let model = self.model;
        let Some(panel) = &model.settings_panel else {
            return Value::Null;
        };
        // Where the cursor's row is on screen, to check that previews never move the panel.
        let shown = model.shown_entries();
        let cursor_line = review_ui::model::settings::lines(&shown)
            .iter()
            .position(|line| match line {
                review_ui::model::settings::Line::Entry(index) => shown
                    .get(*index)
                    .is_some_and(|entry| entry.choice == panel.cursor),
                review_ui::model::settings::Line::Section(_) => false,
            });
        let first = first_row(model, List::Settings);
        let cursor_top = cursor_line.map(|line| {
            i64::from(settings.list.y) + i64::try_from(line).unwrap_or(0)
                - i64::try_from(first).unwrap_or(0)
        });
        json!({
            "width": settings.area.width + settings.edge.map_or(0, |edge| edge.width),
            "list_top": settings.list.y,
            "cursor_top": cursor_top,
        })
    }

    fn text(&self) -> Map<String, Value> {
        let model = self.model;
        let mut entries = Map::new();
        entries.insert(
            "metrics".to_owned(),
            json!({
                "row_height": 1,
                "diff_width": self.diff_width,
                "text_offset": model.diff_text().offset(),
                "text_extent": model.diff_text().extent(),
                "widest_half": model.rows.widest_half,
                "buffer_offset": model.buffer_text.offset(),
                "buffer_extent": model.buffer_text.extent(),
            }),
        );
        entries
    }
}

/// Writes `buffer` to `<path>.txt` as text and to `<path>.ansi` with its colors.
fn shot(buffer: &Buffer, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let with = |extension: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(".");
        name.push(extension);
        std::path::PathBuf::from(name)
    };
    std::fs::write(with("txt"), text(buffer))?;
    std::fs::write(with("ansi"), ansi(buffer))
}

/// The symbols of each row of `buffer` that show, the cells a wide one covers left out.
fn rows(buffer: &Buffer) -> Vec<Vec<&ratatui::buffer::Cell>> {
    let area = buffer.area;
    (area.top()..area.bottom())
        .map(|y| {
            let mut shown = Vec::new();
            let mut covered = 0;
            for x in area.left()..area.right() {
                let Some(cell) = buffer.cell((x, y)) else {
                    continue;
                };
                if covered > 0 {
                    covered -= 1;
                    continue;
                }
                covered = usize::from(cell.cell_width()).saturating_sub(1);
                shown.push(cell);
            }
            shown
        })
        .collect()
}

/// The screen as plain text, one line per row, without trailing blanks.
fn text(buffer: &Buffer) -> String {
    let mut text = String::new();
    for row in rows(buffer) {
        let line: String = row.iter().map(|cell| cell.symbol()).collect();
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

/// The screen with its colors and weights as SGR sequences, each row ending in a reset.
fn ansi(buffer: &Buffer) -> String {
    let mut text = String::new();
    for row in rows(buffer) {
        let mut last = None;
        for cell in row {
            let sgr = sgr(cell);
            if last.as_ref() != Some(&sgr) {
                text.push_str("\x1b[0;");
                text.push_str(&sgr);
                text.push('m');
                last = Some(sgr);
            }
            text.push_str(cell.symbol());
        }
        text.push_str("\x1b[0m\n");
    }
    text
}

/// The SGR parameters of a cell's colors and modifiers.
fn sgr(cell: &ratatui::buffer::Cell) -> String {
    let mut parameters: Vec<String> = Vec::new();
    if cell.modifier.contains(Modifier::BOLD) {
        parameters.push("1".to_owned());
    }
    if cell.modifier.contains(Modifier::ITALIC) {
        parameters.push("3".to_owned());
    }
    if cell.modifier.contains(Modifier::UNDERLINED) {
        parameters.push("4".to_owned());
    }
    if cell.modifier.contains(Modifier::CROSSED_OUT) {
        parameters.push("9".to_owned());
    }
    parameters.extend(color(cell.fg, 38));
    parameters.extend(color(cell.bg, 48));
    parameters.join(";")
}

/// The SGR parameters of `color` as a foreground (`38`) or background (`48`).
fn color(color: Color, base: u8) -> Option<String> {
    match color {
        Color::Rgb(red, green, blue) => Some(format!("{base};2;{red};{green};{blue}")),
        Color::Indexed(index) => Some(format!("{base};5;{index}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Commands parse as review-gui's do, with cells for positions.
    #[test]
    fn commands_parse() {
        assert_eq!(
            Command::parse("key z a"),
            Ok(Command::Key(vec!["z".to_owned(), "a".to_owned()]))
        );
        assert_eq!(
            Command::parse("paste \" \""),
            Ok(Command::Paste(" ".to_owned()))
        );
        assert_eq!(
            Command::parse("click 4 7 2"),
            Ok(Command::Click {
                column: 4,
                row: 7,
                count: 2,
                shift: false,
            })
        );
        assert_eq!(
            Command::parse("wheel -2"),
            Ok(Command::Wheel {
                turns: -2,
                at: None,
                shift: false,
            })
        );
        assert_eq!(
            Command::parse("click 4 7 shift"),
            Ok(Command::Click {
                column: 4,
                row: 7,
                count: 1,
                shift: true,
            })
        );
        assert_eq!(
            Command::parse("drag 1 2 30 4"),
            Ok(Command::Drag([1, 2, 30, 4]))
        );
        assert_eq!(
            Command::parse("resize 80x24"),
            Ok(Command::Resize {
                columns: 80,
                rows: 24
            })
        );
        assert!(Command::parse("paste unquoted").is_err());
        assert!(Command::parse("bogus").is_err());
    }

    /// A shot's text leaves out the cells a wide character covers and trailing blanks, and its
    /// ANSI form carries each run's colors.
    #[test]
    fn shots_show_the_screen() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        buffer.set_string(
            0,
            0,
            "a漢b",
            ratatui::style::Style::default().fg(Color::Rgb(1, 2, 3)),
        );
        assert_eq!(text(&buffer), "a漢b\n");
        let colored = ansi(&buffer);
        assert!(colored.starts_with("\x1b[0;38;2;1;2;3ma漢b"), "{colored:?}");
    }
}
