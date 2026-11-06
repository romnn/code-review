//! Quitting, once what the reviewer did is saved.
//!
//! The app quits only once the marks queued and in flight and the folded folders are saved,
//! and the status line says what it waits for meanwhile.
//! While it waits it takes no new marks, so what it waits for can only shrink.
//! Nothing the reviewer marked is ever dropped, however often they ask.
//!
//! Who asked decides what a failure does:
//!
//! - The reviewer ([`QuitKind::Interactive`]) sees the status line, so a mark that fails to save
//!   stops the quitting and its failure stays in view.
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
    /// Asked to quit; the marks queued and in flight and the folded folders are saved first.
    ///
    /// A failure stops only a [`QuitKind::Interactive`] wait. A reviewer who quits again after
    /// a failure stopped the quitting has seen it, so their quit waits as an unattended one
    /// does.
    Waiting(QuitKind),
    /// A mark failed to save while the reviewer waited to quit, so the app stays with the
    /// failure in view; quitting again waits for the marks still queued without stopping for
    /// another failure, and a new mark makes the next quit an ordinary one again.
    Stopped,
    /// Everything is saved and [`Effect::Quit`] is queued.
    Done,
}

impl Quitting {
    /// Whether the app waits to quit, taking no new marks.
    #[must_use]
    pub fn waits(self) -> bool {
        matches!(self, Self::Waiting(_))
    }
}

impl Model {
    /// Quits once the marks queued and in flight and the folded folders are saved, as `kind`
    /// asks; see the module documentation.
    ///
    /// The update that saves the last of them queues [`Effect::Quit`], and the status line
    /// says what the app waits for meanwhile.
    pub fn quit(&mut self, kind: QuitKind) {
        self.quitting = match (self.quitting, kind) {
            (Quitting::Done, _) => return,
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

    /// Queues [`Effect::Quit`] when the app waits to quit and nothing is left to save, else
    /// says what is, once each time that changes.
    pub(super) fn finish_quitting(&mut self) {
        if !self.quitting.waits() {
            return;
        }
        if !self.marking && self.marks.is_empty() && !self.folders.busy() {
            self.quitting = Quitting::Done;
            self.effect(Effect::Quit);
            return;
        }
        let status = self.quitting_status();
        if status != self.quit_said {
            self.quit_said.clone_from(&status);
            self.set_status(status);
        }
    }

    /// What the status line says while the app waits to quit: what it saves first.
    pub(super) fn quitting_status(&self) -> String {
        let saving = self.marks.len() + usize::from(self.marking);
        if saving > 0 {
            format!("Saving {} before quitting…", plural(saving, "mark"))
        } else {
            "Saving the folded folders before quitting…".to_owned()
        }
    }

    /// What the status line adds to a mark's failure while it keeps the app from quitting: what
    /// quitting again does.
    pub(super) fn stopped_hint(&self) -> &'static str {
        if self.marks.is_empty() {
            "Quitting stopped; quit again to leave anyway."
        } else {
            "Quitting stopped; quit again to leave once the marks still queued are saved."
        }
    }
}
