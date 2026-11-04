//! QA mode: line commands on standard input drive the window for scripted checks and
//! screenshots.
//!
//! Commands, one per line:
//!
//! - `key <keystroke>` types a keystroke through gpui's dispatch, so key bindings apply, for
//!   example `key j` or `key shift-s`.
//! - `action <name>` dispatches an action by name, for example `action review_lite::NextUnit`.
//! - `resize <width>x<height>` asks for a new window size.
//! - `find <text>` moves the cursor to the next row containing `text`, wrapping around.
//! - `scroll <pixels>` scrolls the hunk list by `pixels`, down when positive.
//! - `state` prints the observable UI state as one line of JSON.
//! - `quit` exits.
//!
//! Each command answers with one line on standard output (`ok`, `error: ...`, or the state)
//! only once background work it started (marks, go-to-definition) is done and a frame has been
//! rendered after it, so a screenshot taken after the answer shows the result.

use std::cell::Cell;
use std::io::Write as _;
use std::rc::Rc;
use std::time::Duration;

use gpui::{AsyncWindowContext, Context, Keystroke, WeakEntity, Window, point, px, size};
use serde_json::{Value, json};

use super::{ROW_HEIGHT, ReviewApp};
use crate::rows::{RowKind, state_label};

/// How often QA mode checks whether background work or the next frame is done.
const POLL: Duration = Duration::from_millis(5);

/// A parsed QA command.
enum Command {
    Key(String),
    Action(String),
    Resize { width: f32, height: f32 },
    Find(String),
    Scroll(f32),
    State,
    Quit,
}

impl Command {
    fn parse(line: &str) -> Result<Self, String> {
        let (verb, argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = argument.trim();
        match verb {
            "key" if !argument.is_empty() => Ok(Self::Key(argument.to_owned())),
            "action" if !argument.is_empty() => Ok(Self::Action(argument.to_owned())),
            "resize" => {
                let parsed = argument
                    .split_once('x')
                    .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)));
                let (width, height) = parsed.ok_or("resize takes <width>x<height>")?;
                Ok(Self::Resize { width, height })
            }
            "find" if !argument.is_empty() => Ok(Self::Find(argument.to_owned())),
            "scroll" => argument
                .parse()
                .map(Self::Scroll)
                .map_err(|_| "scroll takes a number of pixels".to_owned()),
            "state" => Ok(Self::State),
            "quit" => Ok(Self::Quit),
            _ => Err(format!("unknown command {line:?}")),
        }
    }
}

/// Starts reading QA commands from standard input.
pub(super) fn start(window: &Window, cx: &mut Context<ReviewApp>) {
    cx.spawn_in(window, async move |this, cx| {
        loop {
            let line = cx
                .background_executor()
                .spawn(async {
                    let mut line = String::new();
                    match std::io::stdin().read_line(&mut line) {
                        Ok(0) | Err(_) => None,
                        Ok(_) => Some(line),
                    }
                })
                .await;
            let Some(line) = line else {
                cx.update(|_, cx| cx.quit()).ok();
                break;
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let quit = line == "quit";
            let reply = run(line, &this, cx).await;
            println!("{reply}");
            std::io::stdout().flush().ok();
            if quit {
                cx.update(|_, cx| cx.quit()).ok();
                break;
            }
        }
    })
    .detach();
}

/// Runs one command and returns its answer, once the window has settled.
async fn run(line: &str, this: &WeakEntity<ReviewApp>, cx: &mut AsyncWindowContext) -> String {
    let command = match Command::parse(line) {
        Ok(command) => command,
        Err(err) => return format!("error: {err}"),
    };
    let done = match &command {
        Command::Key(keystroke) => match Keystroke::parse(keystroke) {
            Ok(keystroke) => cx
                .update(|window, cx| {
                    window.dispatch_keystroke(keystroke, cx);
                })
                .map_err(|err| err.to_string()),
            Err(err) => Err(err.to_string()),
        },
        Command::Action(name) => cx
            .update(|window, cx| {
                let action = cx.build_action(name, None).map_err(|err| err.to_string())?;
                window.dispatch_action(action, cx);
                Ok(())
            })
            .map_err(|err| err.to_string())
            .flatten(),
        Command::Resize { width, height } => cx
            .update(|window, _| window.resize(size(px(*width), px(*height))))
            .map_err(|err| err.to_string()),
        Command::Find(text) => this
            .update(cx, |this, cx| this.qa_find(text, cx))
            .map_err(|err| err.to_string())
            .flatten(),
        Command::Scroll(pixels) => this
            .update(cx, |this, cx| {
                let handle = this.list.0.borrow().base_handle.clone();
                let max = handle.max_offset().y;
                let target = (-handle.offset().y + px(*pixels)).clamp(px(0.0), max);
                handle.set_offset(point(px(0.0), -target));
                cx.notify();
            })
            .map_err(|err| err.to_string()),
        Command::State | Command::Quit => Ok(()),
    };
    if let Err(err) = done {
        return format!("error: {err}");
    }
    if let Err(err) = settle(this, cx).await {
        return format!("error: {err}");
    }
    match command {
        Command::State => this
            .update(cx, |this, _| this.qa_state().to_string())
            .unwrap_or_else(|err| format!("error: {err}")),
        _ => "ok".to_owned(),
    }
}

/// Waits until no background work is pending and a frame has been rendered after it.
async fn settle(this: &WeakEntity<ReviewApp>, cx: &mut AsyncWindowContext) -> Result<(), String> {
    while this
        .update(cx, |this, _| this.busy || this.looking_up)
        .map_err(|err| err.to_string())?
    {
        cx.background_executor().timer(POLL).await;
    }
    let rendered = Rc::new(Cell::new(false));
    let flag = Rc::clone(&rendered);
    this.update_in(cx, |_, window, cx| {
        cx.notify();
        window.on_next_frame(move |_, _| flag.set(true));
    })
    .map_err(|err| err.to_string())?;
    while !rendered.get() {
        cx.background_executor().timer(POLL).await;
    }
    Ok(())
}

impl ReviewApp {
    /// Moves the cursor to the next row containing `text`.
    fn qa_find(&mut self, text: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let count = self.rows.rows.len();
        let found = (1..=count)
            .map(|step| (self.cursor + step) % count)
            .find(|&index| {
                self.rows
                    .rows
                    .get(index)
                    .is_some_and(|row| row.text.contains(text))
            })
            .ok_or_else(|| format!("no row contains {text:?}"))?;
        self.move_cursor(found, cx);
        Ok(())
    }

    /// The observable UI state, as QA mode's `state` command prints it.
    fn qa_state(&self) -> Value {
        let handle = self.list.0.borrow().base_handle.clone();
        let offset = f32::from(handle.offset().y).abs();
        let height = f32::from(handle.bounds().size.height);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "row indices from non-negative pixel offsets"
        )]
        let (first, end) = (
            (offset / ROW_HEIGHT).floor() as usize,
            ((offset + height) / ROW_HEIGHT).ceil() as usize,
        );
        let end = end.min(self.rows.rows.len());
        let view = self.history.current_view().ok();
        let mut visible_units = Vec::new();
        for row in self.rows.rows.get(first..end).unwrap_or_default() {
            if row.kind == RowKind::Header {
                let badge = view
                    .as_ref()
                    .and_then(|view| view.state(row.unit).ok())
                    .map(state_label);
                visible_units.push(json!({ "unit": row.unit.to_string(), "badge": badge }));
            }
        }
        let focused = self.row();
        let steps: Vec<Value> = self
            .partition
            .steps
            .iter()
            .zip(&self.progress)
            .map(|(step, progress)| {
                json!({ "title": step.title, "reviewed": progress.reviewed, "total": progress.total })
            })
            .collect();
        json!({
            "revision": self.partition.revision.to_string(),
            "selected_step": self.selected,
            "step_title": self.selected.and_then(|index| self.partition.steps.get(index)).map(|step| &step.title),
            "steps": steps,
            "row_count": self.rows.rows.len(),
            "cursor_row": self.cursor,
            "focused_unit": focused.map(|row| row.unit.to_string()),
            "focused_hunk": focused
                .and_then(|row| self.rows.hunks.get(row.hunk))
                .map(|units| units.iter().map(ToString::to_string).collect::<Vec<_>>()),
            "token": focused.zip(self.token.clone()).and_then(|(row, token)| row.text.get(token)),
            "scroll_offset": offset,
            "visible_rows": [first, end],
            "visible_units": visible_units,
            "trusted": self.trusted,
            "source": self.source.as_ref().map(|source| source.title.to_string()),
            "status": self.status.to_string(),
        })
    }
}
