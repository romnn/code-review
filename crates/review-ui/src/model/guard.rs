//! What a review key may act on.
//!
//! A held key repeats, and a review key that repeats acts only on what its press began on:
//! never on a change the selection moved to meanwhile, after an approval moved it on, a
//! refresh landed it elsewhere or the press revealed what it now shows.
//! A front end that can tell a repeat from a press says so, as review-gui does from gpui.
//! Where nothing tells, as in a terminal, keys of one review action closer together than
//! [`REPEAT`] are one key held down.

use std::time::{Duration, Instant};

use review_core::{ShownRun, UnitId};

use super::{Action, Model};

/// Keys of one review action closer together than this are one key held down, where the
/// front end cannot tell.
pub(super) const REPEAT: Duration = Duration::from_millis(500);

/// A key the front end delivered, with when it came.
#[derive(Debug, Clone, Copy)]
pub(super) struct KeyInput {
    /// The action it is bound to.
    pub(super) action: Action,
    /// Whether it repeats a key held down: `Some(true)` for a repeat, `Some(false)` for a
    /// press, `None` where the front end cannot tell.
    pub(super) held: Option<bool>,
    /// When it came.
    pub(super) at: Instant,
}

impl KeyInput {
    /// A press of `action` now, as an explicit action is.
    pub(super) fn pressed(action: Action) -> Self {
        Self {
            action,
            held: Some(false),
            at: Instant::now(),
        }
    }
}

/// A review key pressed, and held for as long as it repeats.
#[derive(Debug, Clone)]
pub(super) struct Press {
    action: Action,
    /// When the press began.
    pub(super) started: Instant,
    last: Instant,
    /// What the press began on; `None` once it revealed something, which no repeat acts on.
    pub(super) target: Option<Aim>,
}

/// What a review key acts on, told apart exactly: one change since review as the engine made
/// it, or units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Aim {
    Change(ShownRun),
    Units(Vec<UnitId>),
}

impl Model {
    /// Does what `action` does, as a key the front end delivered: `held` says whether the key
    /// repeats one held down, `None` where the front end cannot tell.
    pub fn key(&mut self, action: Action, held: Option<bool>) {
        self.act(KeyInput {
            action,
            held,
            at: Instant::now(),
        });
    }

    /// Does what the key `input` does; a key of another action ends a review key's press.
    pub(super) fn act(&mut self, input: KeyInput) {
        if self
            .press
            .as_ref()
            .is_some_and(|press| press.action != input.action)
        {
            self.press = None;
        }
        self.input = Some(input);
        self.perform(input.action);
    }

    /// Decides whether the review key in hand acts: a press does, and so does a repeat on what
    /// its press began on; says why it does not otherwise. Every review key passes here first.
    pub(super) fn held_refusal(&mut self) -> Option<String> {
        let KeyInput { action, held, at } = self.input?;
        let target = self.aim();
        let repeat = held.unwrap_or_else(|| {
            self.press.as_ref().is_some_and(|press| {
                press.action == action && at.saturating_duration_since(press.last) < REPEAT
            })
        });
        let refusal = || {
            Some(
                "Not marked: the key is held down from before this showed; press it again."
                    .to_owned(),
            )
        };
        match self.press.as_mut().filter(|_| repeat) {
            Some(press) => {
                press.last = at;
                if press.target.is_none() || press.target != target {
                    return refusal();
                }
            }
            // A repeat of a press made before anything showed acts on nothing.
            None if repeat => {
                self.press = Some(Press {
                    action,
                    started: at,
                    last: at,
                    target: None,
                });
                return refusal();
            }
            None => {
                self.press = Some(Press {
                    action,
                    started: at,
                    last: at,
                    target,
                });
            }
        }
        None
    }

    /// Makes the press in hand one that revealed something, so none of its repeats acts on
    /// what now shows.
    pub(super) fn revealed(&mut self) {
        if let Some(press) = &mut self.press {
            press.target = None;
        }
    }

    /// What the review keys act on now.
    fn aim(&self) -> Option<Aim> {
        if let Some(hunk) = self.target_hunk() {
            return Some(match &hunk.change {
                Some(change) => Aim::Change(change.clone()),
                None => Aim::Units(vec![hunk.unit]),
            });
        }
        self.review_target()
            .ok()
            .map(|(units, _)| Aim::Units(units))
    }
}

#[cfg(test)]
impl Model {
    /// Moves the press in hand `by` into the past, as if the key was let go that long ago.
    pub(super) fn pause_keys(&mut self, by: Duration) {
        if let Some(press) = &mut self.press {
            press.last = press.last.checked_sub(by).unwrap_or(press.last);
            press.started = press.started.checked_sub(by).unwrap_or(press.started);
        }
    }

    /// Holds a review key down since `ago`, on what the keys act on now, repeating until now.
    pub(super) fn hold_since(&mut self, ago: Duration) {
        let now = Instant::now();
        self.press = Some(Press {
            action: Action::ToggleApprove,
            started: now.checked_sub(ago).unwrap_or(now),
            last: now,
            target: self.aim(),
        });
    }
}
