//! The window's keys: every key press goes to review-ui's [`Matcher`], and the actions it
//! resolves run on the model.
//!
//! gpui still dispatches its own key bindings first, and the only ones left are those of
//! gpui-component's text input, while a text field has the focus: Backspace or Left edit the
//! field and stop there, while Escape, Enter (once the field has reported it), Tab, Up and Down
//! go on.
//! A key that goes on reaches [`ReviewApp::key_down`], a capture listener on the element of the
//! window's key context, which hands it to the matcher: a key the matcher claims runs its action
//! or waits for the next key of a sequence, and goes no further; a key it leaves goes on, so a
//! focused text field types it.
//! A tap of a lone modifier, which gpui turns into a keystroke of its own, reaches the matcher
//! through a keystroke interceptor instead, since it is no key-down event.
//! The interceptor also keeps a text field's keys in order while keys wait for a sequence there,
//! such as `;` of a `[keys]` sequence `; t`: a key that goes on with them goes to the matcher
//! before the input's own bindings, and before any other key the waiting ones replay, so
//! Backspace after the `;` deletes it.
//!
//! Keys the matcher replays into a text field are typed into its input and handed to the model
//! at once, in order with the actions around them.
//! A replayed key that types no text, such as `ctrl-x` of a `ctrl-x ctrl-s` sequence the next
//! key does not complete, goes through gpui again once gpui finished the key in hand, and then
//! so does that key, after it: to the input alone when the matcher already left it unclaimed,
//! and through every binding, the matcher's too, when the matcher has not seen it yet, as gpui
//! replays a sequence's keys before it dispatches the key that broke it.
//!
//! The matcher sees the key contexts gpui's element tree has around the focus, mapped to
//! review-ui's areas: the window's root element is [`Area::Window`], and the focused pane,
//! overlay or bar inside it names its area; a text field's own `Input` context is not an area.
//! The focus identity is gpui's focus handle, so keys waiting for a sequence are dropped when
//! the focus moves, as gpui drops its own.

use std::time::Instant;

use gpui::{
    Context, Focusable as _, InputPreference, KeyDownEvent, KeystrokeEvent, Subscription, Task,
    Window,
};
use review_ui::keymap::{Area, Binding};
use review_ui::keys::{
    KeyDown, Keystroke, Matcher, Modifiers, Outcome, Replay, Resolution, Target,
};
use review_ui::model::Action;

use super::ReviewApp;

/// The matcher and what drives it from gpui.
pub(super) struct Keys {
    matcher: Matcher<Option<gpui::FocusHandle>>,
    /// The timer that replays keys waiting for a sequence, while they have a deadline.
    timer: Option<Task<()>>,
    /// Whether a keystroke is being sent through gpui again, which the matcher resolved
    /// already.
    resending: bool,
    /// Whether the keystroke interceptor resolved the key in hand, so the capture listener
    /// leaves it alone.
    intercepted: bool,
    /// Hands lone modifier taps to the matcher while the window lives.
    #[expect(
        dead_code,
        reason = "held for its drop, which ends the interception with the window"
    )]
    taps: Subscription,
}

impl Keys {
    /// The keys of `bindings`, the keymap after the configuration's overrides.
    pub(super) fn new(bindings: &[Binding], cx: &mut Context<ReviewApp>) -> Self {
        let this = cx.entity().downgrade();
        let taps = cx.intercept_keystrokes(move |event, window, cx| {
            let claimed = this
                .update(cx, |this, cx| this.intercept(event, window, cx))
                .unwrap_or(false);
            if claimed {
                cx.stop_propagation();
            }
        });
        Self {
            matcher: Matcher::new(bindings),
            timer: None,
            resending: false,
            intercepted: false,
            taps,
        }
    }
}

/// One thing a resolved key press does, in order.
#[derive(Debug)]
enum Step {
    /// Runs an action on the model.
    Dispatch(Action),
    /// Types the text of a keystroke the matcher left unclaimed into the focused text field.
    Type(String),
    /// Sends a keystroke the matcher left unclaimed, which types no text, through gpui again,
    /// for the focused text field's own bindings.
    Resend(gpui::Keystroke),
    /// Sends a keystroke the matcher has not seen through gpui again, as if it were pressed
    /// now: to the input's bindings and the matcher alike.
    Send(gpui::Keystroke),
}

impl Step {
    /// Whether it sends a keystroke through gpui, which must wait until gpui finished the one
    /// in hand.
    fn sends(&self) -> bool {
        matches!(self, Self::Resend(_) | Self::Send(_))
    }
}

impl ReviewApp {
    /// Hands a key press to the matcher and does what it resolves; a claimed key goes no
    /// further.
    pub(super) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.latency.pending = Some(Instant::now());
        // A resent or intercepted keystroke was resolved before, and a lone modifier comes as a
        // tap.
        if self.keys.resending
            || std::mem::take(&mut self.keys.intercepted)
            || lone_modifier(&event.keystroke)
        {
            return;
        }
        let key = KeyDown {
            keystroke: keystroke(&event.keystroke),
            prefer_character_input: event.prefer_character_input,
        };
        if self.resolve_key(&key, Some(&event.keystroke), window, cx) {
            cx.stop_propagation();
        }
    }

    /// Hands the matcher a keystroke before gpui's bindings see it, when it must: a lone
    /// modifier tap, and a key that goes on with keys waiting for a sequence in a text field.
    /// Before any other key there, the waiting keys replay.
    ///
    /// Returns whether the key goes no further.
    fn intercept(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let pressed = &event.keystroke;
        self.keys.intercepted = false;
        if self.keys.resending {
            return false;
        }
        if lone_modifier(pressed) {
            let key = KeyDown {
                keystroke: keystroke(pressed),
                prefer_character_input: false,
            };
            return self.resolve_key(&key, None, window, cx);
        }
        if !self.text_focused(window, cx) {
            return false;
        }
        let contexts = contexts(window);
        let target = Target {
            contexts: &contexts,
            focus: window.focused(cx),
            takes_text: true,
        };
        let key = KeyDown {
            keystroke: keystroke(pressed),
            prefer_character_input: event.input_preference == InputPreference::CharacterInput,
        };
        if self.keys.matcher.continues(&key.keystroke, &target) {
            self.keys.intercepted = true;
            return self.resolve_key(&key, Some(pressed), window, cx);
        }
        let Flushed { steps, claimed } = flushed(
            replay_steps(self.keys.matcher.flush(&target), true),
            pressed,
        );
        if steps.is_empty() {
            return false;
        }
        self.keys.intercepted = claimed;
        self.run_steps(steps, window, cx);
        self.arm_key_timeout(window, cx);
        claimed
    }

    /// Resolves `key` and does what it resolves to, `sent` being the keystroke gpui dispatches
    /// unless the key is claimed; returns whether it is.
    fn resolve_key(
        &mut self,
        key: &KeyDown,
        sent: Option<&gpui::Keystroke>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let contexts = contexts(window);
        let takes_text = self.text_focused(window, cx);
        let target = Target {
            contexts: &contexts,
            focus: window.focused(cx),
            takes_text,
        };
        let Resolution { replays, outcome } =
            self.keys.matcher.key_down(key, &target, Instant::now());
        let mut steps = replay_steps(replays, takes_text);
        // Typed text lands at once; only resent keystrokes come after the key in hand.
        let resends = steps.iter().any(Step::sends);
        let claimed = match outcome {
            Outcome::Dispatch(action) => {
                steps.push(Step::Dispatch(action));
                true
            }
            Outcome::Pending => true,
            // The key goes on by itself unless keystrokes are resent before it, which it must
            // follow.
            Outcome::Unclaimed => match sent {
                Some(sent) if resends => {
                    steps.push(Step::Resend(sent.clone()));
                    true
                }
                _ => false,
            },
        };
        self.run_steps(steps, window, cx);
        self.arm_key_timeout(window, cx);
        claimed
    }

    /// Replays the keys waiting for a sequence once their deadline passed.
    fn key_timeout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let contexts = contexts(window);
        let takes_text = self.text_focused(window, cx);
        let target = Target {
            contexts: &contexts,
            focus: window.focused(cx),
            takes_text,
        };
        let replays = self.keys.matcher.timeout(&target, Instant::now());
        self.run_steps(replay_steps(replays, takes_text), window, cx);
        self.arm_key_timeout(window, cx);
    }

    /// Starts the timer for the matcher's deadline, replacing the one before.
    fn arm_key_timeout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.keys.timer = self.keys.matcher.deadline().map(|deadline| {
            cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(deadline.saturating_duration_since(Instant::now()))
                    .await;
                this.update_in(cx, ReviewApp::key_timeout).ok();
            })
        });
    }

    /// Runs `steps` in order: at once when they only dispatch actions and type text, and once
    /// gpui finished the event in hand when they resend keystrokes, because gpui dispatches one
    /// key at a time.
    fn run_steps(&mut self, steps: Vec<Step>, window: &mut Window, cx: &mut Context<Self>) {
        if !steps.iter().any(Step::sends) {
            for step in steps {
                match step {
                    Step::Dispatch(action) => self.dispatch(action, window, cx),
                    Step::Type(text) => self.type_text(&text, window, cx),
                    Step::Resend(_) | Step::Send(_) => {}
                }
            }
            return;
        }
        let this = cx.entity();
        window.defer(cx, move |window, cx| {
            for step in steps {
                match step {
                    Step::Dispatch(action) => {
                        this.update(cx, |this, cx| this.dispatch(action, window, cx));
                    }
                    Step::Type(text) => {
                        this.update(cx, |this, cx| this.type_text(&text, window, cx));
                    }
                    Step::Resend(keystroke) => {
                        this.update(cx, |this, _| this.keys.resending = true);
                        window.dispatch_keystroke(keystroke, cx);
                        // The input reports its edit only once gpui flushes its events, after
                        // these steps; the model takes it now, or an action after it would put
                        // the text from before it back into the input.
                        this.update(cx, |this, cx| {
                            this.keys.resending = false;
                            this.take_input_text(window, cx);
                        });
                    }
                    Step::Send(keystroke) => {
                        window.dispatch_keystroke(keystroke, cx);
                        this.update(cx, |this, cx| this.take_input_text(window, cx));
                    }
                }
            }
        });
    }

    /// Types `text` into the focused text field as a key would, replacing its selection, and
    /// hands the field's new text to the model at once.
    fn type_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self
            .typing_field()
            .and_then(|field| self.input(field))
            .cloned()
        else {
            return;
        };
        input.update(cx, |input, cx| input.replace(text, window, cx));
        self.take_input_text(window, cx);
    }

    /// Hands the focused text field's text to the model, as its input reports an edit.
    fn take_input_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(field) = self.typing_field() else {
            return;
        };
        let Some(text) = self
            .input(field)
            .map(|input| input.read(cx).value().to_string())
        else {
            return;
        };
        self.update_model(window, cx, |model| model.edit_field(field, &text));
    }

    /// Whether a text field's input has the focus, taking typed text as gpui's input handler
    /// does.
    fn text_focused(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.inputs
            .iter()
            .any(|input| input.state.focus_handle(cx).is_focused(window))
    }
}

/// What the interceptor does with the key in hand after the keys waiting for a sequence were
/// flushed into `steps`.
struct Flushed {
    /// The steps, in order.
    steps: Vec<Step>,
    /// Whether the key in hand goes no further now, because it is sent again after them.
    claimed: bool,
}

/// The waiting keys' `steps` and then the key in hand, `pressed`: a key sent through gpui again
/// comes after the key in hand, so when the steps send one, `pressed` is sent after them through
/// every binding, the matcher's too, rather than going on now.
fn flushed(mut steps: Vec<Step>, pressed: &gpui::Keystroke) -> Flushed {
    let claimed = steps.iter().any(Step::sends);
    if claimed {
        steps.push(Step::Send(pressed.clone()));
    }
    Flushed { steps, claimed }
}

/// What the replays do: an action, or, when a text field could take it, as gpui replays an
/// unbound keystroke into the focused input, its text typed or a keystroke without text resent;
/// elsewhere nothing takes it.
fn replay_steps(replays: Vec<Replay>, takes_text: bool) -> Vec<Step> {
    replays
        .into_iter()
        .filter_map(|Replay { keystroke, action }| match action {
            Some(action) => Some(Step::Dispatch(action)),
            None if !takes_text => None,
            None => Some(match typed(&keystroke) {
                Some(text) => Step::Type(text.to_owned()),
                None => Step::Resend(gpui_keystroke(&keystroke)),
            }),
        })
        .collect()
}

/// The text `keystroke` types into a text field, when it types any without a shortcut's
/// modifier.
fn typed(keystroke: &Keystroke) -> Option<&str> {
    let Modifiers {
        control, platform, ..
    } = keystroke.modifiers;
    keystroke
        .key_char
        .as_deref()
        .filter(|text| !control && !platform && !text.chars().any(char::is_control))
}

/// The areas of the key contexts around the focus, from the window's root element down.
fn contexts(window: &Window) -> Vec<Area> {
    window
        .context_stack()
        .iter()
        .filter_map(|context| Area::from_context(&context.primary()?.key))
        .collect()
}

/// Whether `keystroke` is a lone modifier, the keystroke gpui makes of a modifier tapped alone.
fn lone_modifier(keystroke: &gpui::Keystroke) -> bool {
    matches!(
        keystroke.key.as_str(),
        "shift" | "control" | "alt" | "platform" | "function"
    )
}

/// The matcher's keystroke of a gpui keystroke, field for field.
fn keystroke(keystroke: &gpui::Keystroke) -> Keystroke {
    let gpui::Modifiers {
        control,
        alt,
        shift,
        platform,
        function,
    } = keystroke.modifiers;
    Keystroke {
        modifiers: Modifiers {
            control,
            alt,
            shift,
            platform,
            function,
        },
        key: keystroke.key.clone(),
        key_char: keystroke.key_char.clone(),
    }
}

/// gpui's keystroke of a matcher keystroke, field for field.
fn gpui_keystroke(keystroke: &Keystroke) -> gpui::Keystroke {
    let Modifiers {
        control,
        alt,
        shift,
        platform,
        function,
    } = keystroke.modifiers;
    gpui::Keystroke {
        modifiers: gpui::Modifiers {
            control,
            alt,
            shift,
            platform,
            function,
        },
        key: keystroke.key.clone(),
        key_char: keystroke.key_char.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use std::time::Instant;

    use gpui::{DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate, KeyContext};
    use review_ui::keymap::{Area, Binding, keymap};
    use review_ui::keys::{
        KeyDown, Keystroke, Matcher, Matches, Modifiers, Outcome, Resolution, Target,
    };
    use review_ui::model::Action;

    use super::{Flushed, Step, flushed, gpui_keystroke, keystroke, replay_steps};

    gpui::actions!(review_gui_test, [Bound]);

    /// gpui's parse of one keystroke in the port's shape, or `None` when gpui rejects it.
    fn gpui_parse(source: &str) -> Option<Keystroke> {
        gpui::Keystroke::parse(source)
            .ok()
            .map(|parsed| keystroke(&parsed))
    }

    /// The ported keystroke grammar accepts and rejects what gpui does and parses the same
    /// modifiers, key and typed character, for every default key and for odd input, so
    /// `[keys]` validation agrees with what gpui binds.
    #[test]
    fn keystrokes_parse_as_gpui_parses_them() {
        let (bindings, _) = keymap(&BTreeMap::new());
        let mut sources: Vec<String> = bindings
            .iter()
            .flat_map(|binding| binding.keys.iter().cloned())
            .collect();
        sources.extend(
            [
                "ctrl-shift-e",
                "shift-g",
                "G",
                "g",
                "space",
                "escape",
                "Escape",
                "ENTER",
                "ctrl--",
                "ctrl-=",
                "alt-shift-?",
                "-",
                "--",
                "ctrl-",
                "a-",
                "",
                " ",
                "foo-bar",
                "bogus-x",
                "ctrl",
                "shift",
                "ctrl-alt",
                "fn",
                "cmd-k",
                "super-k",
                "win-k",
                "secondary-s",
                "CTRL-ALT-SHIFT-FN-x",
                "alt-s->ß",
                "Tab->x",
                "ctrl-S->s",
                "a->",
                "a->b-c",
                "ctrl->",
                "é",
                "É",
                "ctrl-é",
                "ü-x",
                "日本",
                "f12",
                "F12",
                "g g",
                "z shift-m",
                "g  ctrl-d",
                "\tg\n",
            ]
            .map(str::to_owned),
        );
        for source in &sources {
            // Each keystroke on its own.
            let ported = Keystroke::parse(source).ok();
            assert_eq!(ported, gpui_parse(source), "keystroke {source:?}");
            // The sequence, split on whitespace as gpui's key bindings split it.
            let ported = Keystroke::parse_sequence(source).ok();
            let expected: Option<Vec<Keystroke>> =
                source.split_whitespace().map(gpui_parse).collect();
            assert_eq!(ported, expected, "sequence {source:?}");
        }
    }

    /// `bindings` bound in gpui as review-gui once bound them: each key of each entry in
    /// keymap order, in its area's key context, with the action's name as the binding's input.
    fn gpui_keymap(bindings: &[Binding]) -> gpui::Keymap {
        let built = bindings.iter().flat_map(|binding| {
            let predicate = KeyBindingContextPredicate::parse(binding.area.context())
                .map(Rc::new)
                .expect("an area's key context parses");
            binding.keys.iter().filter_map(move |keys| {
                KeyBinding::load(
                    keys,
                    Box::new(Bound),
                    Some(Rc::clone(&predicate)),
                    false,
                    Some(binding.action.into()),
                    &DummyKeyboardMapper,
                )
                .ok()
            })
        });
        gpui::Keymap::new(built.collect())
    }

    /// What gpui's keymap resolves `input` to under `contexts`, in the port's shape.
    fn gpui_matches(
        keymap: &gpui::Keymap,
        input: &[Keystroke],
        contexts: &[KeyContext],
    ) -> Matches {
        let input: Vec<gpui::Keystroke> = input.iter().map(gpui_keystroke).collect();
        let (bindings, pending) = keymap.bindings_for_input(&input, contexts);
        let actions = bindings
            .iter()
            .map(|binding| {
                let name = binding.action_input().unwrap_or_default();
                Action::from_name(&name).expect("the binding names one of the model's actions")
            })
            .collect();
        Matches { actions, pending }
    }

    /// Keystrokes as they are typed: every keystroke the keymap binds, bare and with the
    /// character gpui's simulated typing gives it, and keys typed with characters of their own.
    fn typed_keystrokes(bindings: &[Binding]) -> Vec<Keystroke> {
        let mut typed: Vec<Keystroke> = Vec::new();
        let mut add = |keystroke: Keystroke| {
            if !typed.contains(&keystroke) {
                typed.push(keystroke);
            }
        };
        let sources = bindings
            .iter()
            .flat_map(|binding| binding.keys.iter())
            .flat_map(|keys| keys.split_whitespace())
            .chain([
                "x",
                "shift",
                "control",
                "1",
                "!",
                "?",
                "+",
                "_",
                "|",
                "=",
                ";",
                "h",
                "a",
                "ctrl-+",
                "ctrl-=",
                "shift-tab",
                "shift-enter",
            ]);
        for source in sources {
            let Ok(parsed) = gpui::Keystroke::parse(source) else {
                continue;
            };
            add(keystroke(&parsed));
            add(keystroke(&parsed.with_simulated_ime()));
        }
        // Keys that type a character other than themselves.
        for (source, character) in [
            ("shift-/", "?"),
            ("shift-1", "!"),
            ("shift-=", "+"),
            ("alt-s", "ß"),
            ("ctrl-shift-=", "+"),
            ("shift-g", "g"),
        ] {
            let mut parsed = Keystroke::parse(source).expect("a valid keystroke");
            parsed.key_char = Some(character.to_owned());
            add(parsed);
        }
        typed
    }

    /// The port resolves typed keystrokes to the bindings gpui's own keymap does, in the same
    /// precedence order and with the same decision to wait for more, for every area's context
    /// stack, with and without a text field's `Input` context inside, with the default keymap
    /// and with overrides that make sequences and prefixes compete.
    #[test]
    fn bindings_resolve_as_gpuis_keymap_resolves_them() {
        let overrides: BTreeMap<String, Vec<String>> = [
            ("NextFile", &["g", "n"][..]),
            ("ToggleSidebar", &["ctrl-b", "g x", "space space"]),
            ("ZoomIn", &["z", "ctrl-="]),
            ("FindNext", &["n", "n n n"]),
            ("ToggleTrust", &["ctrl-t", "; t"]),
            ("ToggleOverview", &[]),
            ("CloseWindow", &["?"]),
            // Keys two actions claim in one area, so the later binding wins.
            ("ToggleFlag", &["!", "j"]),
            ("CursorUp", &["k", "w"]),
        ]
        .into_iter()
        .map(|(action, keys)| {
            let keys = keys.iter().map(|&key| key.to_owned()).collect();
            (action.to_owned(), keys)
        })
        .collect();
        let input_context = KeyContext::parse("Input").expect("a valid key context");
        let context_of =
            |area: Area| KeyContext::parse(area.context()).expect("a valid key context");

        let mut stacks: Vec<Vec<Area>> = vec![
            Vec::new(),
            vec![Area::Window],
            vec![Area::Window, Area::Tree, Area::Diff],
            vec![Area::Diff, Area::Window],
            vec![Area::Window, Area::Tree, Area::Window],
        ];
        stacks.extend(Area::ALL.iter().map(|&area| vec![Area::Window, area]));

        let mut waiting = 0;
        let mut competing = 0;
        for overrides in [BTreeMap::new(), overrides] {
            let (bindings, _) = keymap(&overrides);
            let matcher = Matcher::<()>::new(&bindings);
            let gpui = gpui_keymap(&bindings);
            let typed = typed_keystrokes(&bindings);

            // Every keystroke alone, and after each key that begins a sequence.
            let mut inputs: Vec<Vec<Keystroke>> =
                typed.iter().map(|key| vec![key.clone()]).collect();
            for first in ["g", "z", "y", "n", "space", ";"] {
                let first = gpui_parse(first).expect("a valid keystroke");
                inputs.extend(typed.iter().map(|key| vec![first.clone(), key.clone()]));
            }
            inputs.push(
                ["n", "n", "n"]
                    .map(|key| gpui_parse(key).expect("valid"))
                    .to_vec(),
            );
            inputs.push(
                ["n", "n"]
                    .map(|key| gpui_parse(key).expect("valid"))
                    .to_vec(),
            );

            for stack in &stacks {
                let contexts: Vec<KeyContext> =
                    stack.iter().map(|&area| context_of(area)).collect();
                let mut with_input = contexts.clone();
                with_input.push(input_context.clone());
                for input in &inputs {
                    let ported = matcher.bindings_for_input(input, stack);
                    assert_eq!(
                        ported,
                        gpui_matches(&gpui, input, &contexts),
                        "{input:?} in {stack:?}"
                    );
                    assert_eq!(
                        ported,
                        gpui_matches(&gpui, input, &with_input),
                        "{input:?} in {stack:?} with a text field"
                    );
                    if ported.pending {
                        waiting += 1;
                    }
                    if ported.actions.len() > 1 {
                        competing += 1;
                    }
                }
            }
        }

        // The comparison covers keys that wait for a sequence and keys several bindings claim.
        assert!(waiting > 100, "{waiting} inputs wait");
        assert!(
            competing > 100,
            "{competing} inputs complete several bindings"
        );

        // The typed keys cover shifted capitals and characters typed with modifiers.
        let modifiers = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert!(
            typed_keystrokes(&keymap(&BTreeMap::new()).0)
                .iter()
                .any(|key| key.modifiers == modifiers && key.key_char.as_deref() == Some("G"))
        );
    }

    /// A key that breaks a sequence waiting in a text field still runs its own binding once
    /// the sequence's keys replay, as gpui dispatches it after replaying them: with
    /// `CloseWindow` on `ctrl-x ctrl-c`, `ctrl-x` then Escape in a picker's query resends
    /// `ctrl-x` to the query's input and still leaves the query for the list.
    #[test]
    fn a_key_that_breaks_a_sequence_still_runs_its_binding() {
        let overrides = [("CloseWindow".to_owned(), vec!["ctrl-x ctrl-c".to_owned()])]
            .into_iter()
            .collect();
        let (bindings, problems) = keymap(&overrides);
        assert!(problems.is_empty(), "{problems:?}");
        let mut matcher = Matcher::<()>::new(&bindings);
        let contexts = [Area::Window, Area::Picker];
        let target = Target {
            contexts: &contexts,
            focus: (),
            takes_text: true,
        };
        let gpui = |source: &str| gpui::Keystroke::parse(source).expect("a valid keystroke");
        let key = |source: &str| KeyDown {
            keystroke: keystroke(&gpui(source)),
            prefer_character_input: false,
        };
        let now = Instant::now();

        // `ctrl-x` waits for the sequence's second key.
        let Resolution { replays, outcome } = matcher.key_down(&key("ctrl-x"), &target, now);
        assert_eq!(replays, []);
        assert!(matches!(outcome, Outcome::Pending), "{outcome:?}");

        // Escape does not continue the sequence, so the interceptor flushes `ctrl-x`, which
        // types no text and goes back to the query's input alone, and sends Escape after it
        // through every binding.
        assert!(!matcher.continues(&keystroke(&gpui("escape")), &target));
        let Flushed { steps, claimed } =
            flushed(replay_steps(matcher.flush(&target), true), &gpui("escape"));
        assert!(claimed);
        assert!(
            matches!(
                steps.as_slice(),
                [Step::Resend(first), Step::Send(then)]
                    if *first == gpui("ctrl-x") && *then == gpui("escape")
            ),
            "{steps:?}"
        );

        // Sent again, Escape reaches the matcher, which leaves the query for the list.
        let Resolution { replays, outcome } = matcher.key_down(&key("escape"), &target, now);
        assert_eq!(replays, []);
        assert!(
            matches!(outcome, Outcome::Dispatch(Action::PickerBrowse)),
            "{outcome:?}"
        );

        // A flush that only types text or runs actions sends nothing, so the key in hand goes
        // on by itself.
        let Flushed { steps, claimed } = flushed(
            vec![Step::Type("x".to_owned()), Step::Dispatch(Action::Quit)],
            &gpui("escape"),
        );
        assert!(!claimed);
        assert_eq!(steps.len(), 2);
    }
}
