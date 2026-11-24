//! Remembering settings changed in the app in the configuration file, off the UI thread.
//!
//! The model applies a setting at once and queues its edit of the file.
//! Edits are saved one at a time in the order made, each in a write of its own, so none
//! overtakes another and one that fails takes no other with it; quitting waits for them as it
//! does for marks.

use crate::config::{RememberError, Remembered};

use super::effects::{Because, Outcome, Subject};
use super::settings::error_chain;
use super::{Effect, Model};

/// An edit of the configuration file, queued or being saved.
pub(super) struct ConfigEdit {
    /// The values it sets.
    values: Vec<Remembered>,
    /// What the status line says, before the reason, when it cannot be saved.
    failure: String,
}

/// An edit of the configuration file saved, or why it could not be.
pub(super) struct ConfigSaved {
    /// What the status line says, before the reason, when it could not be saved.
    failure: String,
    saved: Result<(), RememberError>,
}

impl Model {
    /// Queues `values` to be written to the configuration file after every edit queued before;
    /// `failure` begins what the status line says when they cannot be.
    ///
    /// Without a configuration file nothing is written, and once quitting is authorized
    /// nothing is either, since the process would exit without the write.
    pub(super) fn save_config(&mut self, values: Vec<Remembered>, failure: String) {
        if self.settings.config_path.is_none() || self.quitting == super::Quitting::Done {
            return;
        }
        self.config_edits.push_back(ConfigEdit { values, failure });
        self.next_config_edit();
    }

    /// Saves the next queued edit of the configuration file in the background, unless one is
    /// being saved.
    fn next_config_edit(&mut self) {
        if self.config_saving {
            return;
        }
        let Some(path) = self.settings.config_path.clone() else {
            return;
        };
        let Some(edit) = self.config_edits.pop_front() else {
            return;
        };
        self.config_saving = true;
        self.spawn(Subject::Always(Because::Save), move || {
            let ConfigEdit { values, failure } = edit;
            let saved = crate::config::remember(&path, &values);
            Outcome::ConfigSaved(ConfigSaved { failure, saved })
        });
    }

    /// Takes in a saved (or failed) edit of the configuration file and starts the next one.
    pub(super) fn config_saved(&mut self, saved: ConfigSaved) {
        let ConfigSaved { failure, saved } = saved;
        if let Err(err) = saved {
            let reason = error_chain(&err);
            // The log keeps it for when the app quits before anyone reads the status line.
            self.effect(Effect::Log(format!(
                "could not save the configuration: {reason}"
            )));
            self.fail(format!("{failure}: {reason}"));
        }
        self.next_config_edit();
    }

    /// Whether edits of the configuration file are queued or being saved.
    pub(super) fn saving_config(&self) -> bool {
        self.config_saving || !self.config_edits.is_empty()
    }
}
