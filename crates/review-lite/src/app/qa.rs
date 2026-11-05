//! QA mode: line commands on standard input drive the window for scripted checks and
//! screenshots.
//!
//! Commands, one per line:
//!
//! - `key <keystrokes>` types keystrokes through gpui's dispatch, so key bindings apply, for
//!   example `key j`, `key shift-s` or `key z a` for a two-key sequence.
//! - `action <name>` dispatches an action by name, for example `action review_lite::NextUnit`.
//! - `click <x> <y> [count] [ctrl] [shift] [right]` presses and releases a mouse button at a
//!   window position; `count` makes it a double or triple click.
//! - `drag <x1> <y1> <x2> <y2>` presses at the first position, renders a frame, moves to the
//!   second in steps, and releases.
//! - `hover <x> <y>` moves the mouse there without a button and waits long enough for a
//!   tooltip to appear.
//! - `hit <x> <y>` names the innermost marked region at a window position and the pointer it
//!   shows, for example `{"region":"close","cursor":"pointer"}`: title-bar buttons, the
//!   title bar, pane dividers and the tree header.
//! - `wheel <dy>` turns the wheel over the hunk list by `dy` pixels, down when positive.
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
//! - `quit` exits.
//! - `rapid <command>` runs a command without waiting for the work it starts, to interleave
//!   input with background work as fast typing does; `rapid state` prints the state at once.
//!
//! Each command (except `sample`) answers with one line on standard output (`ok`,
//! `error: ...`, or the state) only once background work it started (marks, lookups, file
//! loads, scroll animations) is done and a frame has been rendered after it, so a screenshot
//! taken after the answer shows the result.

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
use serde_json::{Value, json};

use super::state::Progress;
use super::{Phase, ReviewApp, scroll};
use crate::rows::Body;

/// A side of a pair as `state` reports it: its kind, both positions and its text, or `null`
/// for padding.
fn qa_cell(cell: Option<&crate::rows::Cell>) -> Value {
    cell.map_or(Value::Null, |cell| {
        json!({
            "kind": format!("{:?}", cell.kind),
            "head": cell.head.as_ref().map(|head| head.line),
            "base": cell.base,
            "text": cell.text.chars().take(60).collect::<String>(),
        })
    })
}

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
    Wheel(f32),
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
            "wheel" => Ok(Self::Wheel(number(words.next(), "a pixel delta")?)),
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
                cx.update(|_, cx| cx.quit()).ok();
                break;
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let quit = line == "quit";
            // `rapid` sends input without waiting for the work it starts, as fast typing does.
            let reply = match line.strip_prefix("rapid ") {
                Some(rest) => run(rest, false, &this, cx).await,
                None => run(line, true, &this, cx).await,
            };
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
                json!({ "scroll_offset": scroll::offset_of(&this.list) }).to_string()
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
        Command::Action(name) => cx
            .update(|window, cx| {
                let action = cx.build_action(name, None).map_err(|err| err.to_string())?;
                window.dispatch_action(action, cx);
                Ok(())
            })
            .map_err(|err| err.to_string())
            .flatten(),
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
        Command::Wheel(dy) => {
            let position = this
                .update(cx, |this, _| this.list.viewport_bounds().center())
                .map_err(|err| err.to_string());
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
            .update(cx, |this, cx| this.qa_find(text, cx))
            .map_err(|err| err.to_string())
            .flatten(),
        Command::File(path) => this
            .update(cx, |this, cx| {
                if !super::state::is_file(&this.tree, path) {
                    return Err(format!("no file {path}"));
                }
                this.show_file(path);
                cx.notify();
                Ok(())
            })
            .map_err(|err| err.to_string())
            .flatten(),
        Command::Scroll(pixels) => this
            .update(cx, |this, cx| {
                let target = scroll::offset_of(&this.list) + pixels;
                let max = f32::from(
                    this.list.content_size().height - this.list.viewport_bounds().size.height,
                );
                scroll::set_offset_of(&this.list, target.clamp(0.0, max.max(0.0)));
                cx.notify();
            })
            .map_err(|err| err.to_string()),
        Command::Pan(pixels) => this
            .update(cx, |this, cx| {
                this.text_scroll.pan(*pixels);
                cx.notify();
            })
            .map_err(|err| err.to_string()),
        Command::Clipboard
        | Command::Hit(..)
        | Command::Repeat { .. }
        | Command::Latency
        | Command::State
        | Command::Quit
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
            .update(cx, |this, _| {
                this.marking
                    || !this.marks.is_empty()
                    || this.looking_up
                    || this.highlighting > 0
                    || this.preview.is_some()
                    || this
                        .picker
                        .as_ref()
                        .is_some_and(super::picker::Picker::busy)
                    || this.refreshing
                    || this.list_animation.is_some()
                    || this.buffer_animation.is_some()
                    || matches!(this.phase, Phase::Loading(_))
            })
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
    /// Selects the hunk at or before the next row of the diff that contains `text`, wrapping
    /// around; the row of a gap or note picks the hunk before it.
    fn qa_find(&mut self, text: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let count = self.rows.rows.len();
        let from = self.hunk_start().unwrap_or(0);
        let found = (1..=count)
            .map(|step| (from + step) % count.max(1))
            .find(|&index| {
                self.rows.rows.get(index).is_some_and(|row| {
                    row.cells().any(|cell| cell.text.contains(text))
                        || row.note().is_some_and(|note| {
                            note.text.contains(text)
                                || note.tag.as_deref().is_some_and(|tag| tag.contains(text))
                        })
                })
            })
            .ok_or_else(|| format!("no row contains {text:?}"))?;
        let hunk = self
            .rows
            .rows
            .get(..=found)
            .unwrap_or_default()
            .iter()
            .rev()
            .find_map(|row| row.hunk)
            .unwrap_or(0);
        self.select_hunk(hunk, cx);
        Ok(())
    }

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
    #[expect(
        clippy::too_many_lines,
        reason = "one JSON document of everything a script can check"
    )]
    fn qa_state(&self) -> Value {
        let row_height = self.fonts.row_height();
        let offset = scroll::offset_of(&self.list);
        let viewport = self.list.viewport_bounds();
        let height = f32::from(viewport.size.height);
        let (first, end) = (
            scroll::whole(offset / row_height),
            scroll::whole(((offset + height) / row_height).ceil()).min(self.rows.rows.len()),
        );
        let view = self.view();
        let hunk_units = self.hunk_units();
        let hunk_badges: Vec<String> = view
            .as_ref()
            .map(|view| {
                super::state::badges(view, &hunk_units, &self.pending)
                    .into_iter()
                    .map(|badge| format!("{badge:?}"))
                    .collect()
            })
            .unwrap_or_default();
        let rows: Vec<Value> = (first..end)
            .filter_map(|index| {
                let row = self.rows.rows.get(index)?;
                // Where a row's code is in each file: one line across the width, or both sides.
                let code = match &row.body {
                    Body::Code(cell) => json!({
                        "head": cell.head.as_ref().map(|head| head.line),
                        "base": cell.base,
                    }),
                    Body::Pair(pair) => json!({
                        "left": qa_cell(pair.left.as_ref()),
                        "right": qa_cell(pair.right.as_ref()),
                    }),
                    Body::Note(_) => Value::Null,
                };
                Some(json!({
                    "row": index,
                    "source": row.source,
                    "kind": format!("{:?}", row.kind()),
                    "hunk": row.hunk,
                    "y": f32::from(viewport.top()) + (scroll::float(index) + 0.5) * row_height - offset,
                    "text": row.text().chars().take(80).collect::<String>(),
                    "tag": row.note().and_then(|note| note.tag.clone()),
                    "scope": row.note().and_then(|note| note.scope.clone()),
                    "code": code,
                }))
            })
            .collect();
        let tree_offset = scroll::offset_of(&self.tree_list);
        let tree_top = f32::from(self.tree_list.viewport_bounds().top());
        let tree: Vec<Value> = self
            .visible_tree()
            .iter()
            .enumerate()
            .filter_map(|(position, &index)| {
                let node = self.tree.get(index)?;
                Some(json!({
                    "path": node.path,
                    "kind": format!("{:?}", node.kind),
                    "change": node.change.as_ref().map(|change| change.kind.letter().to_string()),
                    "progress": format!("{:?}", Progress::of(&node.counts)),
                    "y": tree_top + scroll::float(position) * row_height - tree_offset,
                }))
            })
            .collect();
        let buffer = self.buffer.as_ref().map(|buffer| {
            json!({
                "title": buffer.title(),
                "scope": buffer.scope(),
                "row": buffer.cursor.row,
                "col": buffer.cursor.col,
                "line": buffer.line(buffer.cursor.row).chars().take(80).collect::<String>(),
                "hunk": self.buffer_hunk(),
                "back": buffer.previous.is_some(),
            })
        });
        let look = self.look();
        let view_state = json!({
            "overlay": self.overlay.map(|overlay| format!("{overlay:?}")),
            "token_emphasis": look.token_emphasis,
            "diff_style": look.style.name,
            "theme": look.theme.name,
            "layout": look.layout.name(),
            "effective_layout": self.rows.presentation.layout.name(),
            "density": look.density.name(),
            "effective_density": self.rows.presentation.density.name(),
            "style": {
                "syntax": look.style.syntax,
                "background": look.style.background.name(),
                "headers": look.style.headers.name(),
                "change_bar": look.style.change_bar,
            },
            "applied_theme": self.settings.appearance.applied.theme.name,
            "previewing": self.previewing(),
            "settings": self.qa_settings(),
        });
        let picker = self.picker.as_ref().map(|picker| {
            let entries: Vec<Value> = picker
                .entries
                .iter()
                .take(8)
                .map(|entry| {
                    json!({
                        "path": entry.path,
                        "line": entry.line,
                        "text": entry.text.chars().take(80).collect::<String>(),
                        "changed": entry.change.is_some(),
                    })
                })
                .collect();
            json!({
                "kind": format!("{:?}", picker.kind),
                "scope": format!("{:?}", picker.scope),
                "query": picker.query,
                "editing": picker.editing,
                "searching": picker.searching,
                "count": picker.entries.len(),
                "truncated": picker.truncated,
                "selected": picker.selected,
                "entries": entries,
            })
        });
        let find = self.find.as_ref().map(|find| {
            let current = find.current.and_then(|index| find.matches.get(index));
            json!({
                "target": format!("{:?}", find.target),
                "query": find.query,
                "matches": find.matches.len(),
                "current": find.current,
                "current_column": current.map(|found| format!("{:?}", found.column)),
                "editing": find.editing,
            })
        });
        let filter = self.filter.as_ref().map(|filter| {
            json!({
                "query": filter.query,
                "editing": filter.editing,
            })
        });
        let metrics = json!({
            "row_height": row_height,
            "line_height": self.fonts.line_height,
            "diff_width": self.diff_width,
            "text_offset": self.text_scroll.offset(),
            "text_extent": self.text_scroll.extent(),
            "widest_half": self.rows.widest_half,
        });
        let selection = self.selection.map(|selection| {
            json!({
                "pane": format!("{:?}", selection.pane),
                "column": format!("{:?}", selection.column),
                "anchor": [selection.anchor.row, selection.anchor.col],
                "head": [selection.head.row, selection.head.col],
            })
        });
        json!({
            "phase": match &self.phase {
                Phase::Loading(_) => "loading".to_owned(),
                Phase::Ready => "ready".to_owned(),
                Phase::Failed(err) => format!("failed: {err}"),
            },
            "revision": self.snapshot.as_ref().map(|snapshot| snapshot.revision.to_string()),
            "newer": self.snapshot.as_ref().and_then(super::state::Snapshot::newer).map(|revision| revision.to_string()),
            "mode": format!("{:?}", self.mode),
            "area": format!("{:?}", self.area()),
            "screen": format!("{:?}", self.screen),
            "selected": self.selected,
            "shown": self.rows.files.first().map(|section| section.path.clone()),
            "preview": self.preview.is_some(),
            "highlighted": self
                .rows
                .files
                .first()
                .and_then(|section| self.folds.syntax.get(&section.path))
                .is_some_and(|syntax| syntax.head.is_some() || syntax.base.is_some()),
            "hunk": self.hunk,
            "hunks": self.rows.hunks.len(),
            "hunk_units": hunk_units.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "hunk_badges": hunk_badges,
            "hunk_rows": self.hunk_rows.get(self.hunk).map(|rows| [rows.start, rows.end]),
            "pending": self.pending.len(),
            "queued": self.marks.len() + usize::from(self.marking),
            "row_count": self.rows.rows.len(),
            "rows": rows,
            "visible_rows": [first, end],
            "scroll_offset": offset,
            "row_height": row_height,
            "viewport": [f32::from(viewport.left()), f32::from(viewport.top()), f32::from(viewport.size.width), height],
            "tree": tree,
            "tree_cursor": self.tree_cursor,
            "buffer": buffer,
            "find": find,
            "filter": filter,
            "picker": picker,
            "selection": selection,
            "selected_text": self.selected_text(),
            "trusted": self.trusted,
            "client_decorated": self.client_decorated,
            "window_events": self.regions.events(),
            "layout": {
                "sidebar": self.sidebar_shown().then_some(self.layout.sidebar),
            },
            "view": view_state,
            "font": {
                "family": self.fonts.code.family.to_string(),
                "size": self.fonts.code_size,
                "weight": self.fonts.code.weight.0,
            },
            "metrics": metrics,
            "first_hunk_ms": self.first_hunk.map(|elapsed| elapsed.as_secs_f64() * 1000.0),
            "status": self.status.to_string(),
        })
    }
}
