//! Turning key presses into the keymap's actions, as gpui-pre 0.3.9 dispatches key bindings.
//!
//! The port follows `Keymap::bindings_for_input` (`src/keymap.rs`), `DispatchTree::dispatch_key`,
//! `replay_prefix` and `flush_dispatch` (`src/key_dispatch.rs`), and the keystroke half of
//! `Window::dispatch_key_event` with its pending-input timeout (`src/window.rs`).
//!
//! In gpui's terms, the rules are these:
//!
//! - A binding applies in one [`Area`], the key context it names, and is enabled while that
//!   area is in the context stack, at the depth of its deepest occurrence.
//! - Of the bindings the typed keystrokes complete, the deepest comes first, and of those at
//!   the same depth the one later in the keymap.
//! - Typed keystrokes that begin a longer binding wait for the next key, but only when that
//!   longer binding comes later in the keymap than the first binding they complete.
//! - A key that neither completes nor continues a binding with the waiting keystrokes replays
//!   them: the longest waiting prefix that completes a binding runs its last keystroke's
//!   action, a first keystroke that completes none passes on unclaimed, and the rest are
//!   resolved again with the key.
//! - Waiting keystrokes replay after [`PENDING_TIMEOUT`] when they complete a binding of their
//!   own, when the last one types a character into a focused text field, or when the sequence
//!   had a timeout before; otherwise they wait for the next key however long that takes.
//!   Each keystroke that leaves the sequence waiting restarts the timeout.
//! - Waiting keystrokes belong to the focus they were typed with: a key or a timeout with
//!   another focus drops them without replaying them.
//! - When the platform prefers to type a key's character into a focused text field, a binding
//!   the key completes does not run, and the key types instead.
//!
//! gpui tries a key's bindings in precedence order until an element handles the action; the
//! model handles every action, so the first binding runs.
//! Left out are gpui's `NoAction` and `Unbind` bindings and its precedence of bindings by
//! source, which the keymap has no use for, pausing the timeout, which nothing does, and
//! keyboard mappers, since gpui's Linux one maps nothing.
//! Keystroke interceptors and observers are the front end's, around the matcher.

use std::time::{Duration, Instant};

use super::{KeyDown, Keystroke};
use crate::keymap::{Area, Binding};
use crate::model::Action;

/// How long waiting keystrokes wait for the next key before they replay, when they have a
/// timeout at all; gpui's `PENDING_INPUT_TIMEOUT`.
pub const PENDING_TIMEOUT: Duration = Duration::from_secs(1);

/// Resolves key presses into the keymap's actions, keeping the keystrokes of a sequence typed
/// so far.
///
/// `F` identifies what has the keyboard focus, such as a front end's focus handle; keystrokes
/// typed with one focus never complete a sequence with another.
/// Timeouts follow the clock the caller passes in: after a key leaves keystrokes waiting,
/// [`Self::deadline`] says when to call [`Self::timeout`].
#[derive(Debug, Clone)]
pub struct Matcher<F> {
    /// One binding for each key of each keymap entry, in keymap order.
    bindings: Vec<Bound>,
    /// The keystrokes waiting for the next key.
    pending: Option<Pending<F>>,
}

/// One keystroke sequence bound to an action.
#[derive(Debug, Clone)]
struct Bound {
    keystrokes: Vec<Keystroke>,
    action: Action,
    area: Area,
}

/// Keystrokes waiting for the next key.
#[derive(Debug, Clone)]
struct Pending<F> {
    keystrokes: Vec<Keystroke>,
    /// What had the focus when they were typed.
    focus: F,
    /// When they replay unless a key comes first; `None` waits for the next key.
    deadline: Option<Instant>,
}

/// Where a key press lands.
#[derive(Debug, Clone)]
pub struct Target<'a, F> {
    /// The areas whose key contexts hold the focus, from the window down to the focused one;
    /// a binding applies where its area is among them.
    pub contexts: &'a [Area],
    /// What has the focus.
    pub focus: F,
    /// Whether the focused element takes typed text, as a text field does.
    pub takes_text: bool,
}

/// What a key press does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Keystrokes that waited for this key and form no binding with it, handled first and in
    /// order.
    pub replays: Vec<Replay>,
    /// What the key itself does, after the replays.
    pub outcome: Outcome,
}

/// What a key press itself does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It completes a binding: the action runs, and the key goes no further.
    Dispatch(Action),
    /// It begins or continues a binding: it goes no further and waits for the next key.
    Pending,
    /// No binding claims it: it goes on as an unbound key does, into a focused text field.
    Unclaimed,
}

/// A keystroke that waited for a sequence it did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    /// The keystroke.
    pub keystroke: Keystroke,
    /// The action it runs, as the last keystroke of the longest waiting prefix that completes a
    /// binding; `None` passes it on unclaimed, as [`Outcome::Unclaimed`] does.
    pub action: Option<Action>,
}

/// The bindings typed keystrokes complete, and whether they wait for a longer one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matches {
    /// The actions of the bindings the keystrokes complete, highest precedence first.
    pub actions: Vec<Action>,
    /// Whether a longer binding they begin outranks the first of [`Self::actions`], so they
    /// wait for the next key.
    pub pending: bool,
}

/// One keystroke resolved against the waiting ones, as gpui's `DispatchResult`.
#[derive(Debug, Default)]
struct Dispatched {
    /// The keystrokes now waiting; empty when none wait.
    pending: Vec<Keystroke>,
    /// Whether the waiting keystrokes complete a binding of their own.
    pending_has_binding: bool,
    /// The actions of the bindings the keystroke completes, highest precedence first.
    actions: Vec<Action>,
    replays: Vec<Replay>,
}

impl<F: Clone + PartialEq> Matcher<F> {
    /// A matcher for `bindings`, the keymap after the configuration's overrides.
    ///
    /// Each key of each entry becomes a binding of its own, in keymap order, as gpui bound them
    /// for review-gui; keys that do not parse, which [`crate::keymap::keymap`] reports, are left
    /// out.
    #[must_use]
    pub fn new(bindings: &[Binding]) -> Self {
        let bindings = bindings
            .iter()
            .flat_map(|binding| {
                let action = Action::from_name(binding.action);
                binding.keys.iter().filter_map(move |keys| {
                    Some(Bound {
                        keystrokes: Keystroke::parse_sequence(keys).ok()?,
                        action: action?,
                        area: binding.area,
                    })
                })
            })
            .collect();
        Self {
            bindings,
            pending: None,
        }
    }

    /// Resolves a key press landing on `target` at `now`.
    pub fn key_down(&mut self, key: &KeyDown, target: &Target<'_, F>, now: Instant) -> Resolution {
        // Waiting keystrokes whose timeout passed replay first, as gpui's timer would have
        // fired before the key arrived.
        let mut replays = self.timeout(target, now);
        // Keystrokes typed with another focus never complete a binding.
        let waiting = self
            .pending
            .take()
            .filter(|pending| pending.focus == target.focus);
        let timed = waiting
            .as_ref()
            .is_some_and(|pending| pending.deadline.is_some());
        let keystrokes = waiting
            .map(|pending| pending.keystrokes)
            .unwrap_or_default();

        let dispatched = self.dispatch_key(keystrokes, &key.keystroke, target.contexts);
        replays.extend(dispatched.replays);

        // Waiting
        if !dispatched.pending.is_empty() {
            let types_text = key.keystroke.key_char.is_some() && target.takes_text;
            let timeout = timed || dispatched.pending_has_binding || types_text;
            self.pending = Some(Pending {
                keystrokes: dispatched.pending,
                focus: target.focus.clone(),
                deadline: timeout.then(|| now + PENDING_TIMEOUT),
            });
            return Resolution {
                replays,
                outcome: Outcome::Pending,
            };
        }

        // Complete, or unclaimed
        let character_input = key.prefer_character_input && target.takes_text;
        let outcome = match dispatched.actions.first() {
            Some(&action) if !character_input => Outcome::Dispatch(action),
            _ => Outcome::Unclaimed,
        };
        Resolution { replays, outcome }
    }

    /// Replays the waiting keystrokes once their deadline has passed at `now`, or drops them
    /// when the focus changed since they were typed.
    ///
    /// Returns nothing before the deadline, and when nothing waits or the keystrokes wait for
    /// the next key without a deadline.
    pub fn timeout(&mut self, target: &Target<'_, F>, now: Instant) -> Vec<Replay> {
        let Some(pending) = self
            .pending
            .take_if(|pending| pending.deadline.is_some_and(|deadline| deadline <= now))
        else {
            return Vec::new();
        };
        if pending.focus != target.focus {
            return Vec::new();
        }
        self.replay_all(pending.keystrokes, target.contexts)
    }

    /// When [`Self::timeout`] replays the waiting keystrokes; `None` when nothing waits or it
    /// waits for the next key however long that takes.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().and_then(|pending| pending.deadline)
    }

    /// Whether `keystroke`, landing on `target`, goes on with the waiting keystrokes: completes
    /// a binding with them or waits for a longer one; `false` when nothing waits.
    ///
    /// A front end whose text fields act on some keys before the matcher sees them asks this
    /// first, and replays the waiting keystrokes with [`Self::flush`] before a key that does
    /// not go on with them, so the field gets them in the order they were typed.
    #[must_use]
    pub fn continues(&self, keystroke: &Keystroke, target: &Target<'_, F>) -> bool {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|pending| pending.focus == target.focus)
        else {
            return false;
        };
        let mut input = pending.keystrokes.clone();
        input.push(keystroke.clone());
        let Matches { actions, pending } = self.bindings_for_input(&input, target.contexts);
        pending || !actions.is_empty()
    }

    /// Replays the waiting keystrokes now, as their timeout would, or drops them when the
    /// focus changed since they were typed.
    pub fn flush(&mut self, target: &Target<'_, F>) -> Vec<Replay> {
        match self.pending.take() {
            Some(pending) if pending.focus == target.focus => {
                self.replay_all(pending.keystrokes, target.contexts)
            }
            _ => Vec::new(),
        }
    }

    /// The bindings `input` completes where `contexts` apply, and whether it waits for a
    /// longer one; gpui's `Keymap::bindings_for_input`.
    #[must_use]
    pub fn bindings_for_input(&self, input: &[Keystroke], contexts: &[Area]) -> Matches {
        /// A binding the input completes, with what ranks it.
        struct Complete {
            depth: usize,
            index: usize,
            action: Action,
        }

        let mut complete = Vec::new();
        let mut longer = Vec::new();
        for (index, bound) in self.bindings.iter().enumerate() {
            // The depth of the area's deepest key context, counted from one.
            let Some(depth) = contexts
                .iter()
                .rposition(|area| *area == bound.area)
                .map(|position| position + 1)
            else {
                continue;
            };
            match match_keystrokes(&bound.keystrokes, input) {
                Some(false) => complete.push(Complete {
                    depth,
                    index,
                    action: bound.action,
                }),
                Some(true) => longer.push(index),
                None => {}
            }
        }
        complete.sort_by(|left, right| {
            right
                .depth
                .cmp(&left.depth)
                .then(right.index.cmp(&left.index))
        });
        // A longer binding counts only when it comes later in the keymap than the first
        // binding the input completes.
        let first = complete.first().map(|binding| binding.index);
        let pending = longer
            .iter()
            .any(|&index| first.is_none_or(|first| first < index));
        Matches {
            actions: complete.into_iter().map(|binding| binding.action).collect(),
            pending,
        }
    }

    /// Resolves `keystroke` after the waiting `input`, replaying waiting keystrokes it forms no
    /// binding with; gpui's `DispatchTree::dispatch_key`.
    fn dispatch_key(
        &self,
        mut input: Vec<Keystroke>,
        keystroke: &Keystroke,
        contexts: &[Area],
    ) -> Dispatched {
        input.push(keystroke.clone());
        let Matches { actions, pending } = self.bindings_for_input(&input, contexts);
        if pending {
            return Dispatched {
                pending: input,
                pending_has_binding: !actions.is_empty(),
                ..Dispatched::default()
            };
        }
        if !actions.is_empty() {
            return Dispatched {
                actions,
                ..Dispatched::default()
            };
        }
        if input.len() == 1 {
            return Dispatched::default();
        }

        // Replay a waiting prefix, then resolve the keystroke after the rest.
        input.pop();
        let replay = self.replay_prefix(&mut input, contexts);
        let mut dispatched = self.dispatch_key(input, keystroke, contexts);
        if let Some(replay) = replay {
            dispatched.replays.insert(0, replay);
        }
        dispatched
    }

    /// Takes the longest prefix of `input` that completes a binding off it, as a replay of its
    /// last keystroke with the binding's action, or else its first keystroke, unclaimed;
    /// gpui's `DispatchTree::replay_prefix`.
    ///
    /// Returns `None` only for empty `input`.
    fn replay_prefix(&self, input: &mut Vec<Keystroke>, contexts: &[Area]) -> Option<Replay> {
        for last in (0..input.len()).rev() {
            let Some(prefix) = input.get(..=last) else {
                continue;
            };
            let Matches { actions, .. } = self.bindings_for_input(prefix, contexts);
            if let Some(&action) = actions.first()
                && let Some(keystroke) = input.drain(..=last).next_back()
            {
                return Some(Replay {
                    keystroke,
                    action: Some(action),
                });
            }
        }
        (!input.is_empty()).then(|| Replay {
            keystroke: input.remove(0),
            action: None,
        })
    }

    /// Replays all of `input`, prefix by prefix; gpui's `DispatchTree::flush_dispatch`.
    fn replay_all(&self, mut input: Vec<Keystroke>, contexts: &[Area]) -> Vec<Replay> {
        let mut replays = Vec::new();
        while let Some(replay) = self.replay_prefix(&mut input, contexts) {
            replays.push(replay);
        }
        replays
    }
}

/// Whether `typed` matches the start of `bound`: `None` when it does not, `Some(false)` when
/// it completes it and `Some(true)` when `bound` goes on; gpui's `KeyBinding::match_keystrokes`.
fn match_keystrokes(bound: &[Keystroke], typed: &[Keystroke]) -> Option<bool> {
    if bound.len() < typed.len() {
        return None;
    }
    let matches = bound
        .iter()
        .zip(typed)
        .all(|(bound, typed)| typed.matches(bound));
    matches.then_some(bound.len() > typed.len())
}
