//! Quitting, once what the reviewer did is saved.
//!
//! The app quits only once the marks and decisions on trust queued and in flight, the folded
//! folders and the settings changed in the app are saved, and then the engine stopped: its
//! language servers, its watch and its session; the status line says what it waits for
//! meanwhile.
//! While it waits it takes no new marks or decisions, so what it waits for can only shrink.
//! Once the engine stops, no input acts and no capture shows any more, so nothing the
//! reviewer does then is silently left unsaved.
//! Nothing the reviewer marked is ever dropped, however often they ask.
//!
//! Who asked decides what a failure does:
//!
//! - The reviewer ([`QuitKind::Interactive`]) sees the status line, so a mark that fails to save,
//!   or a revocation of trust that fails to record, stops the quitting and its failure stays in
//!   view.
//!   Asking again while the app waits changes nothing, so a held key or a double click cannot
//!   lose a mark.
//!   Asking again after a failure stopped it waits for the marks still queued as an unattended
//!   quit does, so however many marks fail, two quits leave.
//! - Nobody ([`QuitKind::Unattended`]), as after a hang-up, a termination or input that ended,
//!   can see a failure, so none stops the quitting: the app leaves once every save has
//!   settled, and the log keeps the failures.

use super::{Effect, Model, plural};

/// Who asks the app to quit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitKind {
    /// The reviewer, with a key or the window's close button, who sees the status line.
    Interactive,
    /// Nobody who can see the app any more: a signal, a terminal that hung up or can no longer
    /// be drawn to, or input that ended.
    Unattended,
}

/// Where quitting is; see the module documentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Quitting {
    /// Nobody asked to quit.
    #[default]
    No,
    /// Asked to quit; the marks and decisions on trust queued and in flight, the folded
    /// folders, the place and the settings changed in the app are saved first, and the engine
    /// stopped.
    ///
    /// A failure stops only a [`QuitKind::Interactive`] wait. A reviewer who quits again after
    /// a failure stopped the quitting has seen it, so their quit waits as an unattended one
    /// does.
    Waiting(QuitKind),
    /// A mark failed to save while the reviewer waited to quit, so the app stays with the
    /// failure in view; quitting again waits for the marks still queued without stopping for
    /// another failure, and a new mark makes the next quit an ordinary one again.
    Stopped,
    /// Everything is saved and the engine stops; no input acts and no capture shows.
    Closing,
    /// Everything is saved, the engine stopped and [`Effect::Quit`] is queued.
    Done,
}

impl Quitting {
    /// Whether the app takes no input any more: the engine stops or stopped.
    #[must_use]
    pub fn over(self) -> bool {
        matches!(self, Self::Closing | Self::Done)
    }

    /// Whether the app waits to quit, taking no new marks.
    #[must_use]
    pub fn waits(self) -> bool {
        matches!(self, Self::Waiting(_))
    }
}

impl Model {
    /// Quits once the marks and decisions on trust queued and in flight, the folded folders
    /// and the settings are saved and the engine stopped, as `kind` asks; see the module
    /// documentation.
    ///
    /// The update that takes in the engine's end queues [`Effect::Quit`], and the status line
    /// says what the app waits for meanwhile.
    pub fn quit(&mut self, kind: QuitKind) {
        self.quitting = match (self.quitting, kind) {
            (Quitting::Closing | Quitting::Done, _) => return,
            (Quitting::No, kind) => Quitting::Waiting(kind),
            // The reviewer already waits; asking again only says again what for.
            (waiting @ Quitting::Waiting(_), QuitKind::Interactive) => waiting,
            // Nobody sees a failure, or the reviewer saw one and asked again, so none may stop
            // the quitting any more.
            (Quitting::Waiting(_), QuitKind::Unattended) | (Quitting::Stopped, _) => {
                Quitting::Waiting(QuitKind::Unattended)
            }
        };
        // What the status line says comes from the new state.
        self.quit_said.clear();
    }

    /// Stops the engine when the app waits to quit and nothing is left to save, and queues
    /// [`Effect::Quit`] once it stopped; else says what the app waits for, once each time that
    /// changes.
    pub(super) fn finish_quitting(&mut self) {
        if !self.quitting.waits() {
            return;
        }
        let saving = self.saves_waiting() > 0 || self.folders.busy() || self.saving_config();
        if !saving {
            if self.dispose_engine() {
                self.quitting = Quitting::Closing;
                // Layout, which eases a scroll on, takes no turn any more.
                self.diff_scroll.stop();
                self.buffer_scroll.stop();
                self.set_status("Closing the review before quitting…");
            } else {
                self.closed();
            }
            return;
        }
        let status = self.quitting_status();
        if status != self.quit_said {
            self.quit_said.clone_from(&status);
            self.set_status(status);
        }
    }

    /// Quits once the engine stopped, or at once without one: no input acts any more.
    pub(super) fn closed(&mut self) {
        if self.quitting.waits() || self.quitting == Quitting::Closing {
            self.quitting = Quitting::Done;
            self.effect(Effect::Quit);
        }
    }

    /// What the status line says while the app waits to quit: what it saves first.
    pub(super) fn quitting_status(&self) -> String {
        let saves = self.saves_waiting();
        if saves > 0 {
            format!(
                "Saving {} to the review before quitting…",
                plural(saves, "change")
            )
        } else if self.saving_config() {
            "Saving your settings before quitting…".to_owned()
        } else {
            "Saving the folded folders and your place before quitting…".to_owned()
        }
    }

    /// Takes in a failure the reviewer must see, such as a mark that could not be saved: stops
    /// a quit the reviewer waits for, and returns what the status line adds while the failure
    /// keeps the app from quitting, which is what quitting again does.
    ///
    /// Quitting now would take the failure off screen with the app, unless nobody can see it or
    /// the reviewer saw one and chose to leave anyway.
    pub(super) fn stop_quitting(&mut self) -> Option<&'static str> {
        match self.quitting {
            Quitting::Waiting(QuitKind::Interactive) => self.quitting = Quitting::Stopped,
            Quitting::Stopped => {}
            Quitting::No
            | Quitting::Waiting(QuitKind::Unattended)
            | Quitting::Closing
            | Quitting::Done => {
                return None;
            }
        }
        Some(if self.saves.is_empty() {
            "Quitting stopped; quit again to leave anyway."
        } else {
            "Quitting stopped; quit again to leave once the marks still queued are saved."
        })
    }
}
