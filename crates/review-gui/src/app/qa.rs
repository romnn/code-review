//! QA mode: line commands on standard input drive the window for scripted checks and
//! screenshots.
//!
//! Commands, one per line:
//!
//! - `key <keystrokes>` types keystrokes through gpui's key dispatch, so they reach the key
//!   matcher as typed keys do, for example `key j`, `key shift-s` or `key z a` for a two-key
//!   sequence.
//! - `action <name>` does what one of the model's actions does, as a key bound to it would,
//!   for example `action NextHunk`; a `review_gui::` prefix on the name is ignored.
//! - `click <x> <y> [count] [ctrl] [shift] [right]` presses and releases a mouse button at a
//!   window position; `count` makes it a double or triple click.
//! - `drag <x1> <y1> <x2> <y2>` presses at the first position, renders a frame, moves to the
//!   second in steps, and releases.
//! - `hover <x> <y>` moves the mouse there without a button and waits long enough for a
//!   tooltip to appear.
//! - `hit <x> <y>` names the innermost marked region at a window position and the pointer it
//!   shows, for example `{"region":"close","cursor":"pointer"}`: title-bar buttons, the
//!   title bar, pane dividers and the tree header.
//! - `wheel <dy> [<x> <y>]` turns the wheel by `dy` pixels, down when positive, over the hunk
//!   list or at a window position.
//! - `resize <width>x<height>` asks for a new window size.
//! - `file <path>` shows a file's diff and puts the tree cursor on it, as moving there does.
//! - `find <text>` selects the hunk of the next diff row containing `text`, wrapping around.
//! - `scroll <pixels>` scrolls the hunk list by `pixels`, down when positive.
//! - `pan <pixels>` moves the side-by-side diff's text sideways, left when positive.
//! - `clipboard` prints the clipboard's text as a JSON string.
//! - `latency` prints input-to-frame latency percentiles as JSON.
//! - `repeat <count> <milliseconds> <keystroke>` types a keystroke `count` times, one every
//!   `milliseconds`, as a held key repeats, and prints how long each keystroke's handling took
//!   and the input-to-frame latency over the burst as JSON.
//! - `sample` prints the hunk list's scroll offset at once, without waiting for animations,
//!   to observe a scroll animation in flight.
//! - `state` prints the observable UI state as one line of JSON.
//! - `quit` quits as `q` does: once the marks and folded folders still being saved are saved,
//!   answering first.
//! - `rapid <command>` runs a command without waiting for the work it starts, to interleave
//!   input with background work as fast typing does; `rapid state` prints the state at once.
//!
//! Each command (except `sample`) answers with one line on standard output (`ok`,
//! `error: ...`, or the state) only once background work it started (marks, lookups, file
//! loads, saves, scroll animations) is done and a frame has been rendered after it, so a
//! screenshot taken after the answer shows the result.

use std::cell::Cell;
use std::io::Write as _;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AsyncWindowContext, Context, Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PlatformInput, ScrollDelta, ScrollWheelEvent, WeakEntity, Window, point, px,
    size,
};
use gpui_component::scroll::ScrollbarHandle as _;
use review_ui::frame::List;
use review_ui::model::{Action, QuitKind, Quitting};
use review_ui::state;
use serde_json::{Map, Value, json};

use super::{ReviewApp, scroll};

/// How often QA mode checks whether background work or the next frame is done.
const POLL: Duration = Duration::from_millis(4);

/// How long `hover` waits, a little longer than gpui's tooltip delay.
const TOOLTIP_DELAY: Duration = Duration::from_millis(650);

/// A parsed QA command.
enum Command {
    Key(Vec<String>),
    Repeat {
        count: usize,
        interval: Duration,
        keystroke: String,
    },
    Action(String),
    Click {
        x: f32,
        y: f32,
        count: usize,
        modifiers: Modifiers,
        button: MouseButton,
    },
    Drag([f32; 4]),
    Hover(f32, f32),
    Hit(f32, f32),
    Wheel {
        dy: f32,
        /// Where the pointer is, or the hunk list's center.
        at: Option<(f32, f32)>,
    },
    Resize {
        width: f32,
        height: f32,
    },
    Find(String),
    File(String),
    Scroll(f32),
    Pan(f32),
    Clipboard,
    Latency,
    Sample,
    State,
    Quit,
}

fn number(word: Option<&str>, what: &str) -> Result<f32, String> {
    word.and_then(|word| word.parse().ok())
        .ok_or_else(|| format!("expected {what}"))
}

impl Command {
    fn parse(line: &str) -> Result<Self, String> {
        let (verb, argument) = line.split_once(' ').unwrap_or((line, ""));
        let argument = argument.trim();
        let mut words = argument.split_whitespace();
        match verb {
            "key" if !argument.is_empty() => Ok(Self::Key(
                argument.split_whitespace().map(str::to_owned).collect(),
            )),
            "action" if !argument.is_empty() => Ok(Self::Action(argument.to_owned())),
            "repeat" => {
                let count = number(words.next(), "a count")?;
                let interval = number(words.next(), "an interval in milliseconds")?;
                let keystroke = words.next().ok_or("expected a keystroke")?.to_owned();
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "a small, non-negative count from a QA script"
                )]
                let count = count.max(0.0) as usize;
                Ok(Self::Repeat {
                    count,
                    interval: Duration::from_secs_f32(interval.max(0.0) / 1000.0),
                    keystroke,
                })
            }
            "click" => {
                let x = number(words.next(), "x")?;
                let y = number(words.next(), "y")?;
                let mut count = 1;
                let mut modifiers = Modifiers::default();
                let mut button = MouseButton::Left;
                for word in words {
                    match word {
                        "ctrl" => modifiers.control = true,
                        "shift" => modifiers.shift = true,
                        "right" => button = MouseButton::Right,
                        other => {
                            count = other
                                .parse()
                                .map_err(|_| format!("unknown click option {other}"))?;
                        }
                    }
                }
                Ok(Self::Click {
                    x,
                    y,
                    count,
                    modifiers,
                    button,
                })
            }
            "drag" => Ok(Self::Drag([
                number(words.next(), "x1")?,
                number(words.next(), "y1")?,
                number(words.next(), "x2")?,
                number(words.next(), "y2")?,
            ])),
            "hover" => Ok(Self::Hover(
                number(words.next(), "x")?,
                number(words.next(), "y")?,
            )),
            "hit" => Ok(Self::Hit(
                number(words.next(), "x")?,
                number(words.next(), "y")?,
            )),
            "wheel" => {
                let dy = number(words.next(), "a pixel delta")?;
                let at = match words.next() {
                    Some(x) => Some((number(Some(x), "x")?, number(words.next(), "y")?)),
                    None => None,
                };
                Ok(Self::Wheel { dy, at })
            }
            "resize" => {
                let parsed = argument
                    .split_once('x')
                    .and_then(|(width, height)| Some((width.parse().ok()?, height.parse().ok()?)));
                let (width, height) = parsed.ok_or("resize takes <width>x<height>")?;
                Ok(Self::Resize { width, height })
            }
            "find" if !argument.is_empty() => Ok(Self::Find(argument.to_owned())),
            "file" if !argument.is_empty() => Ok(Self::File(argument.to_owned())),
            "scroll" => Ok(Self::Scroll(number(words.next(), "a number of pixels")?)),
            "pan" => Ok(Self::Pan(number(words.next(), "a number of pixels")?)),
            "clipboard" => Ok(Self::Clipboard),
            "latency" => Ok(Self::Latency),
            "sample" => Ok(Self::Sample),
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
                // The end of the input quits once what the reviewer did is saved, which nobody
                // is left to see; with no command to answer, the model's quit quits at once.
                this.update_in(cx, |this, window, cx| {
                    this.update_model(window, cx, |model| model.quit(QuitKind::Unattended));
                })
                .ok();
                break;
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // The app quits only after the command that made it quit is answered.
            this.update(cx, |this, _| this.answering = true).ok();
            // `rapid` sends input without waiting for the work it starts, as fast typing does.
            let reply = match line.strip_prefix("rapid ") {
                Some(rest) => run(rest, false, &this, cx).await,
                None => run(line, true, &this, cx).await,
            };
            println!("{reply}");
            std::io::stdout().flush().ok();
            let quit = this
                .update(cx, |this, _| {
                    this.answering = false;
                    this.quitting == Quitting::Done
                })
                .unwrap_or(true);
            if quit {
                cx.update(|_, cx| cx.quit()).ok();
                break;
            }
        }
    })
    .detach();
}

/// Dispatches a synthetic platform event to the window.
fn dispatch(cx: &mut AsyncWindowContext, event: PlatformInput) -> Result<(), String> {
    cx.update(|window, cx| {
        window.dispatch_event(event, cx);
    })
    .map_err(|err| err.to_string())
}

/// Presses and releases a mouse button.
fn click(
    cx: &mut AsyncWindowContext,
    position: gpui::Point<gpui::Pixels>,
    button: MouseButton,
    count: usize,
    modifiers: Modifiers,
) -> Result<(), String> {
    dispatch(
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: None,
            modifiers,
        }),
    )?;
    for click_count in 1..=count {
        dispatch(
            cx,
            PlatformInput::MouseDown(MouseDownEvent {
                button,
                position,
                modifiers,
                click_count,
                first_mouse: false,
            }),
        )?;
        dispatch(
            cx,
            PlatformInput::MouseUp(MouseUpEvent {
                button,
                position,
                modifiers,
                click_count,
            }),
        )?;
    }
    Ok(())
}

/// Runs one command and returns its answer, once the window has settled.
#[expect(
    clippy::too_many_lines,
    reason = "one dispatch over every QA command, read top to bottom"
)]
async fn run(
    line: &str,
    wait: bool,
    this: &WeakEntity<ReviewApp>,
    cx: &mut AsyncWindowContext,
) -> String {
    let command = match Command::parse(line) {
        Ok(command) => command,
        Err(err) => return format!("error: {err}"),
    };
    if matches!(command, Command::Sample) {
        return this
            .update(cx, |this, _| {
                json!({ "scroll_offset": this.offset_px(List::Diff) }).to_string()
            })
            .unwrap_or_else(|err| format!("error: {err}"));
    }
    if let Command::Repeat {
        count,
        interval,
        keystroke,
    } = &command
    {
        return repeat(*count, *interval, keystroke, this, cx)
            .await
            .unwrap_or_else(|err| format!("error: {err}"));
    }
    let done: Result<(), String> = match &command {
        Command::Key(keystrokes) => {
            let mut result = Ok(());
            for keystroke in keystrokes {
                result = match Keystroke::parse(keystroke) {
                    Ok(keystroke) => cx
                        .update(|window, cx| {
                            window.dispatch_keystroke(keystroke, cx);
                        })
                        .map_err(|err| err.to_string()),
                    Err(err) => Err(err.to_string()),
                };
                if result.is_err() {
                    break;
                }
            }
            result
        }
        Command::Action(name) => {
            let name = name.strip_prefix("review_gui::").unwrap_or(name);
            match Action::from_name(name) {
                Some(action) => this
                    .update_in(cx, |this, window, cx| this.dispatch(action, window, cx))
                    .map_err(|err| err.to_string()),
                None => Err(format!("unknown action {name:?}")),
            }
        }
        Command::Click {
            x,
            y,
            count,
            modifiers,
            button,
        } => click(cx, point(px(*x), px(*y)), *button, *count, *modifiers),
        Command::Drag([x1, y1, x2, y2]) => {
            let start = point(px(*x1), px(*y1));
            let left = Modifiers::default();
            let mut result = dispatch(
                cx,
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: start,
                    modifiers: left,
                    click_count: 1,
                    first_mouse: false,
                }),
            );
            // A real press is followed by frames before the pointer moves; render one, so
            // state kept only in a frame's closures would be lost here as it is on a desktop.
            if result.is_ok() {
                result = settle(this, cx).await;
            }
            for step in 1..=8 {
                let t = f32::from(u8::try_from(step).unwrap_or(8)) / 8.0;
                let position = point(px(x1 + (x2 - x1) * t), px(y1 + (y2 - y1) * t));
                result = result.and_then(|()| {
                    dispatch(
                        cx,
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position,
                            pressed_button: Some(MouseButton::Left),
                            modifiers: left,
                        }),
                    )
                });
            }
            result.and_then(|()| {
                dispatch(
                    cx,
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: point(px(*x2), px(*y2)),
                        modifiers: left,
                        click_count: 1,
                    }),
                )
            })
        }
        Command::Wheel { dy, at } => {
            let position = match at {
                Some((x, y)) => Ok(point(px(*x), px(*y))),
                None => this
                    .update(cx, |this, _| {
                        this.lists.handles.diff.viewport_bounds().center()
                    })
                    .map_err(|err| err.to_string()),
            };
            position.and_then(|position| {
                dispatch(
                    cx,
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers: Modifiers::default(),
                    }),
                )?;
                dispatch(
                    cx,
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position,
                        delta: ScrollDelta::Pixels(point(px(0.0), px(-dy))),
                        modifiers: Modifiers::default(),
                        touch_phase: gpui::TouchPhase::Moved,
                    }),
                )
            })
        }
        Command::Hover(x, y) => {
            let moved = dispatch(
                cx,
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: point(px(*x), px(*y)),
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                }),
            );
            // gpui shows a tooltip after half a second of hovering.
            cx.background_executor().timer(TOOLTIP_DELAY).await;
            moved
        }
        Command::Resize { width, height } => cx
            .update(|window, _| window.resize(size(px(*width), px(*height))))
            .map_err(|err| err.to_string()),
        Command::Find(text) => this
            .update_in(cx, |this, window, cx| {
                this.update_model(window, cx, |model| model.select_hunk_containing(text))
            })
            .map_err(|err| err.to_string())
            .flatten(),
        Command::File(path) => this
            .update_in(cx, |this, window, cx| {
                if !state::is_file(&this.tree, path) {
                    return Err(format!("no file {path}"));
                }
                this.update_model(window, cx, |model| model.show_file(path));
                Ok(())
            })
            .map_err(|err| err.to_string())
            .flatten(),
        Command::Scroll(pixels) => this
            .update_in(cx, |this, window, cx| {
                let rows = f64::from(*pixels) / f64::from(this.row_px(List::Diff));
                this.update_model(window, cx, |model| model.scroll_list_by(List::Diff, rows));
            })
            .map_err(|err| err.to_string()),
        Command::Pan(pixels) => this
            .update_in(cx, |this, window, cx| {
                let cells = f64::from(*pixels) / f64::from(this.sizes().code_cell());
                this.update_model(window, cx, |model| model.pan_text(cells));
            })
            .map_err(|err| err.to_string()),
        Command::Quit => this
            .update_in(cx, |this, window, cx| {
                this.dispatch(Action::Quit, window, cx);
            })
            .map_err(|err| err.to_string()),
        Command::Clipboard
        | Command::Hit(..)
        | Command::Repeat { .. }
        | Command::Latency
        | Command::State
        | Command::Sample => Ok(()),
    };
    if let Err(err) = done {
        return format!("error: {err}");
    }
    if !wait {
        // `rapid state` shows the state at once, mid-burst.
        if matches!(command, Command::State) {
            return this
                .update(cx, |this, _| this.qa_state().to_string())
                .unwrap_or_else(|err| format!("error: {err}"));
        }
        return "ok".to_owned();
    }
    if let Err(err) = settle(this, cx).await {
        return format!("error: {err}");
    }
    let reply = match command {
        Command::State => this.update(cx, |this, _| this.qa_state().to_string()),
        Command::Clipboard => cx.update(|_, cx| {
            json!(cx.read_from_clipboard().and_then(|item| item.text())).to_string()
        }),
        Command::Latency => this.update(cx, |this, _| this.qa_latency().to_string()),
        Command::Hit(x, y) => this.update(cx, |this, _| {
            let hit = this.regions.hit(point(px(x), px(y)));
            json!({
                "region": hit.map(|(region, _)| region),
                "cursor": hit.map(|(_, cursor)| cursor),
            })
            .to_string()
        }),
        _ => Ok("ok".to_owned()),
    };
    reply.unwrap_or_else(|err| format!("error: {err}"))
}

/// Types `keystroke` `count` times, one every `interval`, as a held key repeats, then waits for
/// the window to settle and reports the handling time of each keystroke and the input-to-frame
/// latency over the burst.
async fn repeat(
    count: usize,
    interval: Duration,
    keystroke: &str,
    this: &WeakEntity<ReviewApp>,
    cx: &mut AsyncWindowContext,
) -> Result<String, String> {
    let keystroke = Keystroke::parse(keystroke).map_err(|err| err.to_string())?;
    this.update(cx, |this, _| this.latency.samples.clear())
        .map_err(|err| err.to_string())?;
    let started = std::time::Instant::now();
    let mut handling = Vec::with_capacity(count);
    for index in 0..count {
        let due = started + interval.mul_f64(f64::from(u32::try_from(index).unwrap_or(u32::MAX)));
        let now = std::time::Instant::now();
        if due > now {
            cx.background_executor().timer(due - now).await;
        }
        let begun = std::time::Instant::now();
        let keystroke = keystroke.clone();
        // The input-to-frame clock starts at the keystroke, as a real key press starts it.
        this.update(cx, |this, _| this.latency.pending = Some(begun))
            .map_err(|err| err.to_string())?;
        cx.update(|window, cx| window.dispatch_keystroke(keystroke, cx))
            .map_err(|err| err.to_string())?;
        handling.push(begun.elapsed());
    }
    let typed = started.elapsed();
    settle(this, cx).await?;
    let settled = started.elapsed();
    let latency = this
        .update(cx, |this, _| this.qa_latency())
        .map_err(|err| err.to_string())?;
    Ok(json!({
        "keys": count,
        "typed_ms": typed.as_secs_f64() * 1000.0,
        "settled_ms": settled.as_secs_f64() * 1000.0,
        "handling": percentiles(&handling),
        "latency": latency,
    })
    .to_string())
}

/// Nearest-rank percentiles of `samples` in milliseconds.
fn percentiles(samples: &[Duration]) -> Value {
    let mut samples: Vec<f64> = samples
        .iter()
        .map(|sample| sample.as_secs_f64() * 1000.0)
        .collect();
    samples.sort_by(f64::total_cmp);
    let at = |percent: usize| {
        let last = samples.len().saturating_sub(1);
        samples.get((last * percent + 50) / 100).copied()
    };
    json!({ "p50_ms": at(50), "p99_ms": at(99), "max_ms": samples.last() })
}

/// Waits until no background work or animation is pending and a frame has been rendered after
/// it.
async fn settle(this: &WeakEntity<ReviewApp>, cx: &mut AsyncWindowContext) -> Result<(), String> {
    loop {
        let busy = this
            .update(cx, |this, _| this.busy() || this.relayout)
            .map_err(|err| err.to_string())?;
        if !busy {
            break;
        }
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
    /// Input-to-frame latency percentiles in milliseconds.
    fn qa_latency(&self) -> Value {
        let mut samples: Vec<f64> = self
            .latency
            .samples
            .iter()
            .map(|sample| sample.as_secs_f64() * 1000.0)
            .collect();
        samples.sort_by(f64::total_cmp);
        // Nearest-rank percentiles: a percent of the last index, in integers.
        let at = |percent: usize| {
            let last = samples.len().saturating_sub(1);
            samples.get((last * percent + 50) / 100).copied()
        };
        json!({ "samples": samples.len(), "p50_ms": at(50), "p99_ms": at(99), "max_ms": samples.last() })
    }

    /// The observable UI state, as QA mode's `state` command prints it.
    fn qa_state(&self) -> Value {
        let row_height = self.sizes().row_height();
        let offset = self.offset_px(List::Diff);
        let viewport = self.lists.handles.diff.viewport_bounds();
        let height = f32::from(viewport.size.height);
        let rows = scroll::whole(offset / row_height)
            ..scroll::whole(((offset + height) / row_height).ceil()).min(self.rows.rows.len());
        review_ui::qa::state(
            self,
            &WindowScreen {
                app: self,
                row_height,
                offset,
                viewport,
                rows,
                tree_offset: self.offset_px(List::Tree),
                tree_top: f32::from(self.lists.handles.tree.viewport_bounds().top()),
            },
        )
    }
}

/// Where things are in the window, in logical pixels, for QA mode's `state`.
struct WindowScreen<'a> {
    app: &'a ReviewApp,
    row_height: f32,
    /// How far the diff is scrolled.
    offset: f32,
    /// The diff list's bounds.
    viewport: gpui::Bounds<gpui::Pixels>,
    /// The diff rows on screen.
    rows: std::ops::Range<usize>,
    /// How far the tree is scrolled.
    tree_offset: f32,
    /// The top of the tree list.
    tree_top: f32,
}

impl review_ui::qa::Screen for WindowScreen<'_> {
    fn rows(&self) -> std::ops::Range<usize> {
        self.rows.clone()
    }

    fn row_y(&self, row: usize) -> Value {
        json!(
            f32::from(self.viewport.top()) + (scroll::float(row) + 0.5) * self.row_height
                - self.offset
        )
    }

    fn tree_y(&self, position: usize) -> Value {
        json!(self.tree_top + scroll::float(position) * self.row_height - self.tree_offset)
    }

    fn list(&self) -> Map<String, Value> {
        let viewport = self.viewport;
        let entries = json!({
            "visible_rows": [self.rows.start, self.rows.end],
            "scroll_offset": self.offset,
            "row_height": self.row_height,
            "viewport": [
                f32::from(viewport.left()),
                f32::from(viewport.top()),
                f32::from(viewport.size.width),
                f32::from(viewport.size.height),
            ],
        });
        object(entries)
    }

    fn window(&self) -> Map<String, Value> {
        let app = self.app;
        object(json!({
            "client_decorated": app.client_decorated,
            "window_events": app.regions.events(),
            "layout": {
                "sidebar": app.sidebar_shown().then_some(app.layout.sidebar),
            },
        }))
    }

    fn settings(&self) -> Value {
        self.app.qa_settings_geometry()
    }

    fn text(&self) -> Map<String, Value> {
        let app = self.app;
        let code_cell = f64::from(app.sizes().code_cell());
        object(json!({
            "font": {
                "family": app.fonts.code.family.to_string(),
                "size": app.code_size,
                "weight": app.fonts.code.weight.0,
            },
            "metrics": {
                "row_height": self.row_height,
                "line_height": app.sizes().line_height,
                "diff_width": app.diff_width,
                "text_offset": scroll::narrow(app.text.offset() * code_cell),
                "text_extent": scroll::narrow(
                    app.text.extent() * f64::from(app.extent_cell)
                ),
                "widest_half": app.rows.widest_half,
            },
            "first_hunk_ms": app.first_hunk.map(|elapsed| elapsed.as_secs_f64() * 1000.0),
        }))
    }
}

/// The entries of `value`, a JSON object.
fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(entries) => entries,
        _ => Map::new(),
    }
}
