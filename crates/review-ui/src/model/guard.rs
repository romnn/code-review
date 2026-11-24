//! What a review key may act on.
//!
//! A held key repeats, and a review key that repeats acts only on what its press began on:
//! never on a change the selection moved to meanwhile, after an approval moved it on, a
//! refresh landed it elsewhere or the press revealed what it now shows.
//! A front end that can tell a repeat from a press says so, as review-gui does from gpui.
//! Where nothing tells, as in a terminal, every key is a press and acts, as in lazygit: quick
//! taps are never mistaken for a held key.

use review_core::{ShownRun, UnitId};

use super::{Action, Model};

/// A key the front end delivered.
#[derive(Debug, Clone, Copy)]
pub(super) struct KeyInput {
    /// The action it is bound to.
    pub(super) action: Action,
    /// Whether the front end reports it as a repeat of a key held down.
    pub(super) held: bool,
}

impl KeyInput {
    /// A press of `action` now, as an explicit action is.
    pub(super) fn pressed(action: Action) -> Self {
        Self {
            action,
            held: false,
        }
    }
}

/// A review key pressed, and held for as long as it repeats.
#[derive(Debug, Clone)]
pub(super) struct Press {
    action: Action,
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
    /// Does what `action` does, as a key the front end delivered: `held` says whether the front
    /// end reports it as a repeat of a key held down, which a terminal never does.
    pub fn key(&mut self, action: Action, held: bool) {
        self.act(KeyInput { action, held });
    }

    /// Does what the key `input` does; a key of another action ends a review key's press.
    ///
    /// Once quitting is authorized no key does anything: the app is going away, and a key then,
    /// such as one a key sequence hands on right after replaying the quit, would start work the
    /// process exits without.
    pub(super) fn act(&mut self, input: KeyInput) {
        if self.quitting == super::Quitting::Done {
            return;
        }
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
        let KeyInput { action, held } = self.input?;
        let target = self.aim();
        let refusal = || {
            Some(
                "Not marked: the key is held down from before this showed; press it again."
                    .to_owned(),
            )
        };
        match self.press.as_mut().filter(|_| held) {
            Some(press) => {
                if press.target.is_none() || press.target != target {
                    return refusal();
                }
            }
            // A repeat of a press made before anything showed acts on nothing.
            None if held => {
                self.press = Some(Press {
                    action,
                    target: None,
                });
                return refusal();
            }
            None => {
                self.press = Some(Press { action, target });
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
    /// Holds a review key down on what the keys act on now.
    pub(super) fn hold(&mut self) {
        self.press = Some(Press {
            action: Action::ToggleApprove,
            target: self.aim(),
        });
    }
}
