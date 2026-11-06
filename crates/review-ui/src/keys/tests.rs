//! The keystroke grammar and the matcher's behavior: sequences, replays, timeouts, context
//! precedence, typing into text fields, `[keys]` overrides and focus changes, after gpui's own
//! key dispatch tests.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::{
    KeyDown, Keystroke, Matcher, Matches, Modifiers, Outcome, PENDING_TIMEOUT, Replay, Resolution,
    Target,
};
use crate::keymap::{Area, Binding, keymap};
use crate::model::Action;

/// A keystroke as gpui's Linux platform types it: shifted letters type their capital, other
/// keys with neither control nor alt type themselves, and named keys type nothing apart from a
/// space.
fn typed(source: &str) -> Keystroke {
    let mut keystroke = Keystroke::parse(source).expect("a valid keystroke");
    let Modifiers {
        control,
        alt,
        shift,
        platform,
        function,
    } = keystroke.modifiers;
    if keystroke.key_char.is_none() && !control && !alt && !platform && !function {
        keystroke.key_char = match keystroke.key.as_str() {
            "space" => Some(" ".to_owned()),
            key if key.chars().count() == 1 && shift => Some(key.to_uppercase()),
            key if key.chars().count() == 1 => Some(key.to_owned()),
            _ => None,
        };
    }
    keystroke
}

/// A key press of `source`, typed as gpui's Linux platform types it.
fn press(source: &str) -> KeyDown {
    KeyDown {
        keystroke: typed(source),
        prefer_character_input: false,
    }
}

/// One binding of `action` to `keys` in `area`, for keymaps written out in a test.
fn binding(action: &'static str, keys: &[&str], area: Area) -> Binding {
    Binding {
        action,
        keys: keys.iter().map(|&key| key.to_owned()).collect(),
        area,
        group: "",
        help: "",
        hint: None,
        pair: None,
    }
}

/// A matcher over the default keymap after `overrides`.
fn defaults(overrides: &[(&str, &[&str])]) -> Matcher<Area> {
    let overrides: BTreeMap<String, Vec<String>> = overrides
        .iter()
        .map(|&(action, keys)| {
            (
                action.to_owned(),
                keys.iter().map(|&key| key.to_owned()).collect(),
            )
        })
        .collect();
    let (bindings, problems) = keymap(&overrides);
    assert_eq!(problems, Vec::<String>::new());
    Matcher::new(&bindings)
}

/// A keyboard in a front end: the matcher, a clock and where the keys land.
struct Keyboard {
    matcher: Matcher<Area>,
    now: Instant,
    /// The focused area, which is also the focus.
    area: Area,
    /// Whether a text field has the focus.
    typing: bool,
}

impl Keyboard {
    fn new(matcher: Matcher<Area>, area: Area) -> Self {
        Self {
            matcher,
            now: Instant::now(),
            area,
            typing: false,
        }
    }

    /// Types the keystrokes of `keys` one after another, returning what the last one did.
    fn keys(&mut self, keys: &str) -> Resolution {
        let mut last = None;
        for key in keys.split_whitespace() {
            let contexts = [Area::Window, self.area];
            let target = Target {
                contexts: &contexts,
                focus: self.area,
                takes_text: self.typing,
            };
            last = Some(self.matcher.key_down(&press(key), &target, self.now));
        }
        last.expect("at least one key")
    }

    /// What the last keystroke of `keys` does itself.
    fn outcome(&mut self, keys: &str) -> Outcome {
        self.keys(keys).outcome
    }

    /// Lets `elapsed` pass and fires the timeout, returning what it replayed.
    fn wait(&mut self, elapsed: Duration) -> Vec<Replay> {
        self.now += elapsed;
        let contexts = [Area::Window, self.area];
        let target = Target {
            contexts: &contexts,
            focus: self.area,
            takes_text: self.typing,
        };
        self.matcher.timeout(&target, self.now)
    }
}

/// A replay of `key` running `action`, or unclaimed.
fn replay(key: &str, action: Option<Action>) -> Replay {
    Replay {
        keystroke: typed(key),
        action,
    }
}

/// Every action the default keymap binds exists in the model.
#[test]
fn default_bindings_name_the_models_actions() {
    let (bindings, _) = keymap(&BTreeMap::new());
    for binding in &bindings {
        assert!(
            Action::from_name(binding.action).is_some(),
            "{}",
            binding.action
        );
    }
}

/// The two-key sequences wait after their first key, without a timeout because that key binds
/// nothing alone, and run their action on the second.
#[test]
fn sequences_complete_on_their_last_key() {
    let mut keyboard = Keyboard::new(defaults(&[]), Area::Diff);
    for (keys, action) in [
        ("g g", Action::FirstHunk),
        ("z a", Action::ToggleFold),
        ("z shift-a", Action::ToggleApproved),
        ("y i", Action::YankUnitId),
        ("shift-g", Action::LastHunk),
    ] {
        let strokes: Vec<&str> = keys.split_whitespace().collect();
        let Some((last, waiting)) = strokes.split_last() else {
            continue;
        };
        for key in waiting {
            assert_eq!(keyboard.outcome(key), Outcome::Pending, "{keys}");
            assert_eq!(keyboard.matcher.deadline(), None, "{keys} has no timeout");
        }
        assert_eq!(
            keyboard.keys(last),
            Resolution {
                replays: Vec::new(),
                outcome: Outcome::Dispatch(action),
            },
            "{keys}"
        );
    }

    // The buffer has its own sequences on the same first key.
    keyboard.area = Area::Buffer;
    assert_eq!(
        keyboard.outcome("g g"),
        Outcome::Dispatch(Action::BufferTop)
    );
    assert_eq!(
        keyboard.outcome("g d"),
        Outcome::Dispatch(Action::GoToDefinition)
    );

    // The tree's fold sequences take shifted letters, typed as capitals.
    keyboard.area = Area::Tree;
    assert_eq!(
        keyboard.outcome("z shift-m"),
        Outcome::Dispatch(Action::CloseAllFolds)
    );
    assert_eq!(
        keyboard.outcome("z shift-r"),
        Outcome::Dispatch(Action::OpenAllFolds)
    );
}

/// A key that continues no sequence replays the waiting key, which binds nothing alone and
/// passes on, and then does what it does alone.
#[test]
fn a_key_that_continues_nothing_replays_the_waiting_ones() {
    let mut keyboard = Keyboard::new(defaults(&[]), Area::Diff);
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    assert_eq!(
        keyboard.keys("j"),
        Resolution {
            replays: vec![replay("g", None)],
            outcome: Outcome::Dispatch(Action::NextHunk),
        }
    );

    // Unbound after the replay too.
    assert_eq!(keyboard.outcome("z"), Outcome::Pending);
    assert_eq!(
        keyboard.keys("x"),
        Resolution {
            replays: vec![replay("z", None)],
            outcome: Outcome::Unclaimed,
        }
    );

    // A replayed key can begin a sequence again: `g z a` replays `g`, then `z` waits.
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    assert_eq!(
        keyboard.keys("z"),
        Resolution {
            replays: vec![replay("g", None)],
            outcome: Outcome::Pending,
        }
    );
    assert_eq!(keyboard.outcome("a"), Outcome::Dispatch(Action::ToggleFold));

    // A lone modifier, as tapping shift types it, ends a sequence like any other key.
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    assert_eq!(
        keyboard.keys("shift"),
        Resolution {
            replays: vec![replay("g", None)],
            outcome: Outcome::Unclaimed,
        }
    );
}

/// Without a binding of the first key alone, a sequence waits for its second key however long
/// that takes, as gpui's does.
#[test]
fn a_prefix_without_a_binding_waits_without_a_timeout() {
    let mut keyboard = Keyboard::new(defaults(&[]), Area::Diff);
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    assert_eq!(keyboard.matcher.deadline(), None);
    assert_eq!(keyboard.wait(Duration::from_secs(60)), Vec::new());
    assert_eq!(keyboard.outcome("g"), Outcome::Dispatch(Action::FirstHunk));
}

/// When the first key binds an action alone, the sequence waits a second for the next key,
/// then runs the shorter binding; a key before then replays it at once.
#[test]
fn a_prefix_with_a_binding_runs_it_after_the_timeout() {
    let matcher = Matcher::new(&[
        binding("NextFile", &["g"], Area::Diff),
        binding("FirstHunk", &["g g"], Area::Diff),
    ]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    let typed_at = keyboard.now;
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    assert_eq!(
        keyboard.matcher.deadline(),
        Some(typed_at + PENDING_TIMEOUT)
    );

    // Not yet
    assert_eq!(keyboard.wait(Duration::from_millis(999)), Vec::new());
    // Then the waiting key runs its own binding.
    assert_eq!(
        keyboard.wait(Duration::from_millis(1)),
        vec![replay("g", Some(Action::NextFile))]
    );
    assert_eq!(keyboard.matcher.deadline(), None);

    // Another key before the timeout replays the binding first.
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    assert_eq!(
        keyboard.keys("x"),
        Resolution {
            replays: vec![replay("g", Some(Action::NextFile))],
            outcome: Outcome::Unclaimed,
        }
    );

    // A key arriving after a deadline nobody fired replays the waiting keys first, as gpui's
    // timer would have fired before the key.
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    keyboard.now += PENDING_TIMEOUT;
    assert_eq!(
        keyboard.keys("g"),
        Resolution {
            replays: vec![replay("g", Some(Action::NextFile))],
            outcome: Outcome::Pending,
        }
    );
}

/// Each key that leaves a timed sequence waiting restarts the timeout, and a sequence keeps its
/// timeout once it had one; it replays the longest prefix that forms a binding.
#[test]
fn the_timeout_restarts_with_each_key_of_a_timed_sequence() {
    let matcher = Matcher::new(&[
        binding("ToggleFold", &["z"], Area::Diff),
        binding("NextHunk", &["z h"], Area::Diff),
        binding("FirstHunk", &["z h j"], Area::Diff),
    ]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    assert_eq!(keyboard.outcome("z"), Outcome::Pending);
    keyboard.now += PENDING_TIMEOUT * 4 / 5;
    assert_eq!(keyboard.outcome("h"), Outcome::Pending);
    assert_eq!(
        keyboard.matcher.deadline(),
        Some(keyboard.now + PENDING_TIMEOUT),
        "the second key restarts the timeout"
    );
    assert_eq!(keyboard.wait(PENDING_TIMEOUT / 5), Vec::new());
    assert_eq!(
        keyboard.wait(PENDING_TIMEOUT * 4 / 5),
        vec![replay("h", Some(Action::NextHunk))],
        "`z h` is the longest prefix with a binding"
    );

    // Without a binding of `z` alone, `z` waits untimed, and `z h` then starts the timeout.
    let matcher = Matcher::new(&[
        binding("NextHunk", &["z h"], Area::Diff),
        binding("FirstHunk", &["z h j"], Area::Diff),
    ]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    assert_eq!(keyboard.outcome("z"), Outcome::Pending);
    assert_eq!(keyboard.matcher.deadline(), None);
    assert_eq!(keyboard.outcome("h"), Outcome::Pending);
    assert_eq!(
        keyboard.matcher.deadline(),
        Some(keyboard.now + PENDING_TIMEOUT)
    );
}

/// A flush replays prefix by prefix: a waiting key that binds nothing passes on, and the rest
/// are resolved again.
#[test]
fn the_timeout_replays_prefix_by_prefix() {
    let matcher = Matcher::new(&[
        binding("NextHunk", &["h"], Area::Diff),
        binding("FirstHunk", &["x h j"], Area::Diff),
        binding("LastHunk", &["x"], Area::Window),
    ]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    // The window's `x` comes later in the keymap than the sequence, so it runs at once.
    assert_eq!(keyboard.outcome("x"), Outcome::Dispatch(Action::LastHunk));

    // With the sequence later, `x` waits, and so does `x h`; the timeout replays `x` with its
    // own binding, then resolves `h` alone.
    let matcher = Matcher::new(&[
        binding("NextHunk", &["h"], Area::Diff),
        binding("LastHunk", &["x"], Area::Window),
        binding("FirstHunk", &["x h j"], Area::Diff),
    ]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    assert_eq!(keyboard.outcome("x"), Outcome::Pending);
    assert_eq!(keyboard.outcome("h"), Outcome::Pending);
    assert_eq!(
        keyboard.wait(PENDING_TIMEOUT),
        vec![
            replay("x", Some(Action::LastHunk)),
            replay("h", Some(Action::NextHunk)),
        ]
    );
}

/// A binding in the focused area outranks the window's on the same key, and of two bindings in
/// one area the later in the keymap wins.
#[test]
fn deeper_contexts_and_later_bindings_win() {
    let matcher = defaults(&[("ToggleSidebar", &["j"])]);
    let mut keyboard = Keyboard::new(matcher, Area::Tree);
    assert_eq!(keyboard.outcome("j"), Outcome::Dispatch(Action::TreeDown));
    // The settings search binds only keys that type nothing, so the window's `j` reaches it.
    keyboard.area = Area::SettingsSearch;
    assert_eq!(
        keyboard.outcome("j"),
        Outcome::Dispatch(Action::ToggleSidebar)
    );

    let matcher = Matcher::<Area>::new(&[
        binding("NextHunk", &["x"], Area::Diff),
        binding("PreviousHunk", &["x"], Area::Diff),
        binding("ToggleTrust", &["x"], Area::Window),
    ]);
    let contexts = [Area::Window, Area::Diff];
    assert_eq!(
        matcher.bindings_for_input(&[typed("x")], &contexts),
        Matches {
            actions: vec![Action::PreviousHunk, Action::NextHunk, Action::ToggleTrust],
            pending: false,
        }
    );

    // The deepest occurrence of an area counts.
    let nested = [Area::Diff, Area::Window];
    assert_eq!(
        matcher.bindings_for_input(&[typed("x")], &nested).actions,
        [Action::ToggleTrust, Action::PreviousHunk, Action::NextHunk]
    );

    // Bindings of areas outside the stack never apply.
    assert_eq!(
        matcher.bindings_for_input(&[typed("x")], &[Area::Tree]),
        Matches {
            actions: Vec::new(),
            pending: false,
        }
    );
}

/// A sequence outranks a shorter binding only when it comes later in the keymap, whatever
/// their areas.
#[test]
fn a_sequence_waits_only_when_it_comes_later_in_the_keymap() {
    let earlier = Matcher::<Area>::new(&[
        binding("ToggleSidebar", &["g x"], Area::Window),
        binding("NextFile", &["g"], Area::Diff),
    ]);
    let contexts = [Area::Window, Area::Diff];
    assert_eq!(
        earlier.bindings_for_input(&[typed("g")], &contexts),
        Matches {
            actions: vec![Action::NextFile],
            pending: false,
        }
    );
    let later = Matcher::<Area>::new(&[
        binding("NextFile", &["g"], Area::Diff),
        binding("ToggleSidebar", &["g x"], Area::Window),
    ]);
    assert_eq!(
        later.bindings_for_input(&[typed("g")], &contexts),
        Matches {
            actions: vec![Action::NextFile],
            pending: true,
        }
    );
}

/// In a text field, keys no binding claims pass on to type, including characters bound in the
/// modes; keys bound in the field's area or the window still run.
#[test]
fn text_fields_type_what_no_binding_claims() {
    let mut keyboard = Keyboard::new(defaults(&[]), Area::Picker);
    keyboard.typing = true;
    for key in [
        "j", "k", "space", "/", "?", "!", "g", "z", "shift-g", "q", "=",
    ] {
        assert_eq!(
            keyboard.keys(key),
            Resolution {
                replays: Vec::new(),
                outcome: Outcome::Unclaimed,
            },
            "{key}"
        );
    }
    for (key, action) in [
        ("escape", Action::PickerBrowse),
        ("enter", Action::PickerOpen),
        ("down", Action::PickerNext),
        ("ctrl-n", Action::PickerNext),
        ("ctrl-t", Action::ToggleTrust),
        ("ctrl-b", Action::ToggleSidebar),
        ("ctrl-q", Action::Quit),
    ] {
        assert_eq!(keyboard.outcome(key), Outcome::Dispatch(action), "{key}");
    }

    // The settings search and the find bar keep their own keys.
    keyboard.area = Area::SettingsSearch;
    assert_eq!(keyboard.outcome("j"), Outcome::Unclaimed);
    assert_eq!(
        keyboard.outcome("escape"),
        Outcome::Dispatch(Action::SettingsBrowse)
    );
    keyboard.area = Area::Find;
    assert_eq!(keyboard.outcome("n"), Outcome::Unclaimed);
    assert_eq!(
        keyboard.outcome("escape"),
        Outcome::Dispatch(Action::CloseFind)
    );
}

/// A key the platform would rather type into a focused text field types, bound or not; away
/// from text fields it runs its binding.
#[test]
fn preferred_character_input_types_into_text_fields() {
    let mut matcher = defaults(&[]);
    let mut key = press("ctrl-t");
    key.prefer_character_input = true;
    let contexts = [Area::Window, Area::Picker];
    let mut target = Target {
        contexts: &contexts,
        focus: Area::Picker,
        takes_text: true,
    };
    assert_eq!(
        matcher.key_down(&key, &target, Instant::now()).outcome,
        Outcome::Unclaimed
    );
    target.takes_text = false;
    assert_eq!(
        matcher.key_down(&key, &target, Instant::now()).outcome,
        Outcome::Dispatch(Action::ToggleTrust)
    );
}

/// In a text field, a character beginning a sequence waits a second and then types, as gpui
/// gives printable keys a timeout there.
#[test]
fn a_typed_character_waits_a_second_in_a_text_field() {
    let matcher = defaults(&[("ToggleTrust", &["ctrl-t", "; t"])]);
    let mut keyboard = Keyboard::new(matcher, Area::Picker);
    keyboard.typing = true;
    assert_eq!(keyboard.outcome(";"), Outcome::Pending);
    assert_eq!(
        keyboard.matcher.deadline(),
        Some(keyboard.now + PENDING_TIMEOUT)
    );
    assert_eq!(keyboard.wait(PENDING_TIMEOUT), vec![replay(";", None)]);

    // Completed in time, it runs; continued otherwise, it types and so does the key.
    assert_eq!(
        keyboard.outcome("; t"),
        Outcome::Dispatch(Action::ToggleTrust)
    );
    assert_eq!(keyboard.outcome(";"), Outcome::Pending);
    assert_eq!(
        keyboard.keys("a"),
        Resolution {
            replays: vec![replay(";", None)],
            outcome: Outcome::Unclaimed,
        }
    );

    // Away from text fields, the same character waits for the next key.
    keyboard.area = Area::Tree;
    keyboard.typing = false;
    assert_eq!(keyboard.outcome(";"), Outcome::Pending);
    assert_eq!(keyboard.matcher.deadline(), None);
}

/// A text field's front end can tell whether a key goes on with the waiting ones before the
/// field's own bindings act on it, and replay the waiting ones first when it does not, so
/// Backspace after a waiting `;` deletes the `;`.
#[test]
fn waiting_keys_flush_before_a_key_that_does_not_continue_them() {
    let matcher = defaults(&[("ToggleTrust", &["ctrl-t", "; t"])]);
    let mut keyboard = Keyboard::new(matcher, Area::Picker);
    keyboard.typing = true;
    let contexts = [Area::Window, Area::Picker];
    let target = Target {
        contexts: &contexts,
        focus: Area::Picker,
        takes_text: true,
    };
    assert!(
        !keyboard.matcher.continues(&typed("t"), &target),
        "nothing waits"
    );

    assert_eq!(keyboard.outcome(";"), Outcome::Pending);
    assert!(keyboard.matcher.continues(&typed("t"), &target));
    assert!(!keyboard.matcher.continues(&typed("backspace"), &target));
    // Another focus never continues them.
    let elsewhere = Target {
        focus: Area::Find,
        ..target
    };
    assert!(!keyboard.matcher.continues(&typed("t"), &elsewhere));

    assert_eq!(keyboard.matcher.flush(&target), vec![replay(";", None)]);
    assert_eq!(
        keyboard.matcher.deadline(),
        None,
        "nothing waits after a flush"
    );
    assert_eq!(keyboard.matcher.flush(&target), Vec::new());
}

/// `[keys]` overrides replace an action's keys everywhere, and an empty list unbinds it.
#[test]
fn overrides_rebind_and_unbind() {
    let matcher = defaults(&[("NextHunk", &["ctrl-j"]), ("ToggleOverview", &[])]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    assert_eq!(keyboard.outcome("j"), Outcome::Unclaimed);
    assert_eq!(
        keyboard.outcome("ctrl-j"),
        Outcome::Dispatch(Action::NextHunk)
    );
    assert_eq!(keyboard.outcome("i"), Outcome::Unclaimed);

    // A sequence on a key bound alone waits only when it comes later in the keymap than that
    // binding: `FirstHunk` comes before `ToggleApprove`, so space approves at once.
    let matcher = defaults(&[("FirstHunk", &["space g"])]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    assert_eq!(keyboard.outcome("g"), Outcome::Unclaimed);
    assert_eq!(
        keyboard.outcome("space"),
        Outcome::Dispatch(Action::ToggleApprove)
    );

    // `ToggleApproved` comes after it, so space then waits, with a timeout because it binds an
    // action alone, and either sequence completes.
    let matcher = defaults(&[
        ("FirstHunk", &["space g"]),
        ("ToggleApproved", &["space a"]),
    ]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    assert_eq!(keyboard.outcome("space"), Outcome::Pending);
    assert_eq!(
        keyboard.matcher.deadline(),
        Some(keyboard.now + PENDING_TIMEOUT)
    );
    assert_eq!(
        keyboard.outcome("a"),
        Outcome::Dispatch(Action::ToggleApproved)
    );
    assert_eq!(
        keyboard.outcome("space g"),
        Outcome::Dispatch(Action::FirstHunk)
    );
    assert_eq!(
        keyboard.wait(Duration::ZERO),
        Vec::new(),
        "nothing waits after a completed sequence"
    );
    assert_eq!(keyboard.outcome("space"), Outcome::Pending);
    assert_eq!(
        keyboard.wait(PENDING_TIMEOUT),
        vec![replay("space", Some(Action::ToggleApprove))]
    );
}

/// Keys waiting with one focus are dropped, never replayed, when a key or the timeout comes
/// with another.
#[test]
fn a_focus_change_drops_waiting_keys() {
    let mut keyboard = Keyboard::new(defaults(&[]), Area::Diff);
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    keyboard.area = Area::Buffer;
    assert_eq!(
        keyboard.keys("d"),
        Resolution {
            replays: Vec::new(),
            outcome: Outcome::Unclaimed,
        },
        "`g` from the diff does not form `g d` in the buffer"
    );

    let matcher = Matcher::new(&[
        binding("NextFile", &["g"], Area::Diff),
        binding("FirstHunk", &["g g"], Area::Diff),
        binding("NextFile", &["g"], Area::Tree),
    ]);
    let mut keyboard = Keyboard::new(matcher, Area::Diff);
    assert_eq!(keyboard.outcome("g"), Outcome::Pending);
    keyboard.area = Area::Tree;
    assert_eq!(keyboard.wait(PENDING_TIMEOUT), Vec::new());
    assert_eq!(keyboard.matcher.deadline(), None, "dropped");
}

/// A typed character other than the key matches a binding of that character, with at most
/// control and platform held, as a layout typing `?` with `shift-/` or `ß` with `alt-s` does.
#[test]
fn typed_characters_match_bindings_of_the_character() {
    let mut question = Keystroke::parse("shift-/").expect("a valid keystroke");
    question.key_char = Some("?".to_owned());
    let mut sharp = Keystroke::parse("alt-s").expect("a valid keystroke");
    sharp.key_char = Some("ß".to_owned());
    let bound = |source| Keystroke::parse(source).expect("a valid keystroke");

    assert!(question.matches(&bound("?")));
    assert!(question.matches(&bound("shift-/")));
    assert!(!question.matches(&bound("/")));
    assert!(sharp.matches(&bound("ß")));
    assert!(sharp.matches(&bound("alt-s")));
    assert!(!sharp.matches(&bound("alt-ß")));

    // A capital typed with shift is the shifted letter, never the capital as a key.
    let capital = typed("shift-g");
    assert!(capital.matches(&bound("shift-g")));
    assert!(capital.matches(&bound("G")));
    assert!(!capital.matches(&bound("g")));
}

/// Modifiers, shifted capitals, the minus key and lone modifiers parse as gpui parses them.
#[test]
fn keystrokes_parse_like_gpui() {
    let parsed = Keystroke::parse("ctrl-shift-e").unwrap();
    assert_eq!(
        parsed.modifiers,
        Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        }
    );
    assert_eq!(parsed.key, "e");

    // A capital is shift and the lowercase letter.
    let capital = Keystroke::parse("G").unwrap();
    assert!(capital.modifiers.shift);
    assert_eq!(capital.key, "g");

    // A trailing `-` is the minus key.
    assert_eq!(Keystroke::parse("ctrl--").unwrap().key, "-");

    // A lone modifier is the key.
    let lone = Keystroke::parse("shift").unwrap();
    assert_eq!(lone.key, "shift");
    assert!(!lone.modifiers.shift);

    // A key followed by more is an error, unless it is a typed character.
    assert!(Keystroke::parse("foo-bar").is_err());
    let typed = Keystroke::parse("alt-s->ß").unwrap();
    assert_eq!(typed.key, "s");
    assert_eq!(typed.key_char.as_deref(), Some("ß"));

    // Sequences split on whitespace.
    assert_eq!(Keystroke::parse_sequence("g  g").unwrap().len(), 2);
    assert!(Keystroke::parse_sequence("g bogus-x").is_err());
}
