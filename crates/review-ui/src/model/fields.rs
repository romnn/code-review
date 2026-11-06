//! The text fields the reviewer types into: the tree's filter, the diff's or the buffer's find,
//! the picker's query and the settings search.
//!
//! The model owns each field's text and whether it takes typing; a front end edits the text
//! with its own widget and reports every change with [`Model::edit_field`] and Enter with
//! [`Model::submit_field`].
//! [`Model::fields`] lists the open fields, which the front end reconciles its widgets with:
//! a field with a new [`FieldState::id`] opened afresh and gets a fresh widget, and a field no
//! longer listed closed.

use super::{Mode, Model};

/// A text field of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    /// The tree's `/` filter.
    Filter,
    /// The `/` search of the diff or the buffer.
    Find,
    /// The open picker's query.
    PickerQuery,
    /// The settings list's search.
    SettingsSearch,
}

/// An open text field as a front end draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldState {
    /// Which field it is.
    pub field: Field,
    /// Unique for the session, and new whenever the field opens afresh, so a front end knows
    /// to start a new widget with its text.
    pub id: u64,
    /// What the field holds.
    pub text: String,
    /// What the field says while it is empty.
    pub placeholder: &'static str,
    /// Whether the field takes typing now.
    pub typing: bool,
}

impl Model {
    /// The open text fields.
    #[must_use]
    pub fn fields(&self) -> Vec<FieldState> {
        let mut fields = Vec::new();
        if let Some(filter) = &self.filter {
            fields.push(FieldState {
                field: Field::Filter,
                id: filter.id,
                text: filter.query.clone(),
                placeholder: "Filter files by path",
                typing: self.typing_field() == Some(Field::Filter),
            });
        }
        if let Some(find) = &self.find {
            fields.push(FieldState {
                field: Field::Find,
                id: find.id,
                text: find.query.clone(),
                placeholder: if find.target == Mode::Diff {
                    "Find in this diff"
                } else {
                    "Find in this file"
                },
                typing: self.typing_field() == Some(Field::Find),
            });
        }
        if let Some(picker) = &self.picker {
            fields.push(FieldState {
                field: Field::PickerQuery,
                id: picker.id,
                text: picker.query.clone(),
                placeholder: match picker.kind {
                    super::picker::Kind::Files => "Find a file",
                    super::picker::Kind::Content => "Search file contents",
                },
                typing: self.typing_field() == Some(Field::PickerQuery),
            });
        }
        if let Some(panel) = &self.settings_panel {
            fields.push(FieldState {
                field: Field::SettingsSearch,
                id: panel.id,
                text: panel.query.clone(),
                placeholder: "Search settings",
                typing: self.typing_field() == Some(Field::SettingsSearch),
            });
        }
        fields
    }

    /// The field that takes typing now, which owns the keyboard; `None` while keys go to a
    /// mode or a list.
    #[must_use]
    pub fn typing_field(&self) -> Option<Field> {
        match self.overlay {
            Some(super::Overlay::Picker) => self
                .picker
                .as_ref()
                .is_some_and(|picker| picker.editing)
                .then_some(Field::PickerQuery),
            Some(super::Overlay::Settings) => {
                self.settings_searching().then_some(Field::SettingsSearch)
            }
            Some(_) => None,
            None if !self.typing() => None,
            None if self.mode == Mode::Tree => Some(Field::Filter),
            None => Some(Field::Find),
        }
    }

    /// The text of `field` changed to `text`, as the reviewer typed it.
    ///
    /// Nothing happens when the text is what the field holds already, so a front end that
    /// echoes the model's text back never searches again.
    pub fn edit_field(&mut self, field: Field, text: &str) {
        match field {
            Field::Filter => {
                let Some(filter) = self.filter.as_mut().filter(|filter| filter.query != text)
                else {
                    return;
                };
                text.clone_into(&mut filter.query);
                self.apply_filter();
            }
            Field::Find => {
                let Some(find) = self.find.as_mut().filter(|find| find.query != text) else {
                    return;
                };
                text.clone_into(&mut find.query);
                let target = find.target;
                self.refresh_find(target);
                self.find_step(true, true);
            }
            Field::PickerQuery => {
                let Some(picker) = self.picker.as_mut().filter(|picker| picker.query != text)
                else {
                    return;
                };
                text.clone_into(&mut picker.query);
                let delay = picker.kind.debounce();
                self.run_picker(delay);
            }
            Field::SettingsSearch => {
                let Some(panel) = self
                    .settings_panel
                    .as_mut()
                    .filter(|panel| panel.query != text)
                else {
                    return;
                };
                text.clone_into(&mut panel.query);
                self.settings_cursor_moved();
            }
        }
    }

    /// Enter in `field`: the filter stays and the tree takes the keys (an empty one goes), a
    /// search stays for `n` and `N` (an empty one goes), and the picker opens its entry.
    /// The settings search applies through its key binding instead.
    pub fn submit_field(&mut self, field: Field) {
        match field {
            Field::Filter => {
                if self
                    .filter
                    .as_ref()
                    .is_some_and(|filter| filter.query.is_empty())
                {
                    self.filter = None;
                } else if let Some(filter) = &mut self.filter {
                    filter.editing = false;
                }
            }
            Field::Find => {
                if self.find.as_ref().is_some_and(|find| find.query.is_empty()) {
                    self.find = None;
                } else if let Some(find) = &mut self.find {
                    find.editing = false;
                }
            }
            Field::PickerQuery => self.picker_open(),
            Field::SettingsSearch => {}
        }
    }

    /// A fresh text field identity.
    pub(super) fn next_field_id(&mut self) -> u64 {
        self.field_ids += 1;
        self.field_ids
    }
}
