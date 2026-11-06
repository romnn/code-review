//! A one-line text editor for a front end without text fields of its own, such as a terminal.
//!
//! The model owns each field's text (see [`crate::model::Field`]); the editor keeps the cursor
//! of the field taking typing, turns editing keys and typed text into the field's new text, and
//! the front end hands that to [`Model::edit_field`](crate::model::Model::edit_field).
//!
//! The editing keys are those of the text input review-gui uses (gpui-component's `Input`, as
//! bound outside macOS), with the readline keys a terminal's users expect:
//!
//! | Keys | Edit |
//! |---|---|
//! | `backspace`, `shift-backspace` | delete the character before the cursor |
//! | `delete`, `shift-delete` | delete the character after the cursor |
//! | `ctrl-backspace`, `alt-backspace`, `ctrl-w` | delete to the start of the word before |
//! | `ctrl-delete`, `alt-delete` | delete to the end of the word after |
//! | `ctrl-u` | delete to the start of the line |
//! | `left`, `right` | move one character |
//! | `ctrl-left`, `alt-left`, `ctrl-right`, `alt-right` | move one word |
//! | `home`, `ctrl-a`, `end`, `ctrl-e` | move to the start or the end |
//!
//! `ctrl-w` and `ctrl-u` are readline's; gpui-component's input leaves them to the window, where
//! `ctrl-w` closes it, which a terminal cannot do.
//! `ctrl-a` selects everything in gpui-component's input; with no selection in a terminal it
//! moves to the start, as readline's does.
//! Everything else, Enter, Escape, Tab and the arrows up and down included, is not an edit and
//! goes on to the keymap, as it does past gpui-component's input.

use crate::keys::{Keystroke, Modifiers};
use crate::model::{Field, FieldState};

/// The field being edited, its text as the editor last saw it, and the cursor in it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineEdit {
    /// The field and its identity, `None` before the first field opens.
    field: Option<(Field, u64)>,
    text: String,
    /// The cursor, a byte offset on a character boundary of [`Self::text`].
    cursor: usize,
}

impl LineEdit {
    /// Follows `field`, the field taking typing: a field opened afresh, or text the model
    /// changed by itself, puts the cursor at the end of the text, where typing adds to it.
    pub fn follow(&mut self, field: &FieldState) {
        let same = self.field == Some((field.field, field.id));
        if same && self.text == field.text {
            return;
        }
        self.field = Some((field.field, field.id));
        self.text.clone_from(&field.text);
        self.cursor = self.text.len();
    }

    /// The text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor, as a byte offset into [`Self::text`].
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Types `text` at the cursor, such as a typed character or a paste; control characters
    /// become spaces, since the field holds one line.
    pub fn insert(&mut self, text: &str) {
        let line: String = text
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        self.text.insert_str(self.cursor, &line);
        self.cursor += line.len();
    }

    /// Does what the editing key `key` does; returns `None` for a key that is no edit, which
    /// goes on to the keymap.
    pub fn key(&mut self, key: &Keystroke) -> Option<Edited> {
        let Modifiers {
            control,
            alt,
            shift,
            platform,
            function,
        } = key.modifiers;
        if platform || function {
            return None;
        }
        let word = (control || alt) && !shift;
        let plain = !control && !alt;
        let before = self.text.clone();
        match key.key.as_str() {
            "backspace" if word => self.delete_to(self.word_start()),
            "backspace" if plain => self.delete_to(self.previous()),
            "delete" if word => self.delete_to(self.word_end()),
            "delete" if plain => self.delete_to(self.next()),
            "w" if control && !alt && !shift => self.delete_to(self.word_start()),
            "u" if control && !alt && !shift => self.delete_to(0),
            "left" if word => self.cursor = self.word_start(),
            "left" if plain && !shift => self.cursor = self.previous(),
            "right" if word => self.cursor = self.word_end(),
            "right" if plain && !shift => self.cursor = self.next(),
            "home" if plain && !shift => self.cursor = 0,
            "a" if control && !alt && !shift => self.cursor = 0,
            "end" if plain && !shift => self.cursor = self.text.len(),
            "e" if control && !alt && !shift => self.cursor = self.text.len(),
            _ => return None,
        }
        Some(if self.text == before {
            Edited::Moved
        } else {
            Edited::Text
        })
    }

    /// Deletes from the cursor to `to`, on whichever side of it `to` is.
    fn delete_to(&mut self, to: usize) {
        let (start, end) = if to < self.cursor {
            (to, self.cursor)
        } else {
            (self.cursor, to)
        };
        self.text.replace_range(start..end, "");
        self.cursor = start;
    }

    /// The character boundary before the cursor.
    fn previous(&self) -> usize {
        self.text
            .get(..self.cursor)
            .and_then(|before| before.char_indices().next_back())
            .map_or(0, |(index, _)| index)
    }

    /// The character boundary after the cursor.
    fn next(&self) -> usize {
        self.text
            .get(self.cursor..)
            .and_then(|after| after.chars().next())
            .map_or(self.cursor, |next| self.cursor + next.len_utf8())
    }

    /// The start of the word before the cursor: past any spaces, then past one run of word
    /// characters or of other characters.
    fn word_start(&self) -> usize {
        let before = self.text.get(..self.cursor).unwrap_or_default();
        let mut characters = before.char_indices().rev().peekable();
        while characters
            .next_if(|(_, character)| character.is_whitespace())
            .is_some()
        {}
        let Some(&(mut start, first)) = characters.peek() else {
            return 0;
        };
        let class = Class::of(first);
        while let Some((index, _)) =
            characters.next_if(|&(_, character)| Class::of(character) == class)
        {
            start = index;
        }
        start
    }

    /// The end of the word after the cursor: past any spaces, then past one run of word
    /// characters or of other characters.
    fn word_end(&self) -> usize {
        let after = self.text.get(self.cursor..).unwrap_or_default();
        let mut characters = after.char_indices().peekable();
        while characters
            .next_if(|(_, character)| character.is_whitespace())
            .is_some()
        {}
        let Some(&(_, first)) = characters.peek() else {
            return self.text.len();
        };
        let class = Class::of(first);
        let mut end = self.text.len();
        while let Some(&(index, character)) = characters.peek() {
            if Class::of(character) != class {
                end = self.cursor + index;
                break;
            }
            characters.next();
        }
        end
    }
}

/// What an editing key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edited {
    /// The cursor moved; the text is as it was.
    Moved,
    /// The text changed, which the model must hear of.
    Text,
}

/// The kinds of characters a word is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Letters, digits and `_`.
    Word,
    /// Spaces.
    Space,
    /// Everything else, such as punctuation.
    Other,
}

impl Class {
    fn of(character: char) -> Self {
        if character.is_alphanumeric() || character == '_' {
            Self::Word
        } else if character.is_whitespace() {
            Self::Space
        } else {
            Self::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An editor on `text` with the cursor at `cursor`.
    fn at(text: &str, cursor: usize) -> LineEdit {
        LineEdit {
            field: Some((Field::Find, 1)),
            text: text.to_owned(),
            cursor,
        }
    }

    fn key(source: &str) -> Keystroke {
        Keystroke::parse(source).unwrap()
    }

    /// Characters go in at the cursor, and Backspace and Delete take one character on either
    /// side of it, a whole one even when it takes several bytes.
    #[test]
    fn typing_and_deleting_characters() {
        let mut edit = at("héllo", 3);
        edit.insert("X");
        assert_eq!((edit.text(), edit.cursor()), ("héXllo", 4));
        assert_eq!(edit.key(&key("backspace")), Some(Edited::Text));
        assert_eq!(edit.key(&key("backspace")), Some(Edited::Text));
        assert_eq!((edit.text(), edit.cursor()), ("hllo", 1));
        assert_eq!(edit.key(&key("delete")), Some(Edited::Text));
        assert_eq!((edit.text(), edit.cursor()), ("hlo", 1));
        // Nothing before the start or after the end.
        let mut edit = at("ab", 0);
        assert_eq!(edit.key(&key("backspace")), Some(Edited::Moved));
        let mut edit = at("ab", 2);
        assert_eq!(edit.key(&key("delete")), Some(Edited::Moved));
        assert_eq!(edit.text(), "ab");
    }

    /// A paste's line breaks and tabs become spaces, so the field keeps one line.
    #[test]
    fn pastes_stay_on_one_line() {
        let mut edit = at("", 0);
        edit.insert("a\nb\tc");
        assert_eq!(edit.text(), "a b c");
    }

    /// Word deletes and moves skip spaces, then one run of word characters or of punctuation.
    #[test]
    fn words_end_at_spaces_and_punctuation() {
        let text = "fn apply_batch(self, x)";
        let mut edit = at(text, text.len());
        assert_eq!(edit.key(&key("ctrl-w")), Some(Edited::Text));
        assert_eq!(edit.text(), "fn apply_batch(self, x");
        assert_eq!(edit.key(&key("ctrl-backspace")), Some(Edited::Text));
        assert_eq!(edit.text(), "fn apply_batch(self, ");
        assert_eq!(edit.key(&key("alt-backspace")), Some(Edited::Text));
        assert_eq!(edit.text(), "fn apply_batch(self");
        assert_eq!(edit.key(&key("ctrl-left")), Some(Edited::Moved));
        assert_eq!(edit.cursor(), "fn apply_batch(".len());
        assert_eq!(edit.key(&key("ctrl-left")), Some(Edited::Moved));
        assert_eq!(edit.cursor(), "fn apply_batch".len());
        assert_eq!(edit.key(&key("ctrl-left")), Some(Edited::Moved));
        assert_eq!(edit.cursor(), "fn ".len());
        assert_eq!(edit.key(&key("ctrl-right")), Some(Edited::Moved));
        assert_eq!(edit.cursor(), "fn apply_batch".len());
        assert_eq!(edit.key(&key("ctrl-delete")), Some(Edited::Text));
        assert_eq!(edit.text(), "fn apply_batchself");
    }

    /// Home, End and their readline keys move to the ends; Ctrl+U deletes to the start.
    #[test]
    fn line_ends() {
        let mut edit = at("balance", 3);
        assert_eq!(edit.key(&key("end")), Some(Edited::Moved));
        assert_eq!(edit.cursor(), 7);
        assert_eq!(edit.key(&key("ctrl-a")), Some(Edited::Moved));
        assert_eq!(edit.cursor(), 0);
        assert_eq!(edit.key(&key("ctrl-e")), Some(Edited::Moved));
        edit.key(&key("left"));
        edit.key(&key("left"));
        assert_eq!(edit.key(&key("ctrl-u")), Some(Edited::Text));
        assert_eq!((edit.text(), edit.cursor()), ("ce", 0));
    }

    /// Keys that edit nothing go on to the keymap: Enter, Escape, Tab, up and down, letters,
    /// and shortcuts such as Ctrl+B.
    #[test]
    fn other_keys_are_no_edits() {
        let mut edit = at("x", 1);
        for source in [
            "enter", "escape", "tab", "up", "down", "j", "ctrl-b", "ctrl-q",
        ] {
            assert_eq!(edit.key(&key(source)), None, "{source}");
        }
        assert_eq!(edit.text(), "x");
    }

    /// A field opened afresh, or text the model replaced, starts with the cursor at its end; the
    /// editor's own text echoed back keeps the cursor where it is.
    #[test]
    fn following_a_field() {
        let state = |id, text: &str| FieldState {
            field: Field::Filter,
            id,
            text: text.to_owned(),
            placeholder: "",
            typing: true,
        };
        let mut edit = LineEdit::default();
        edit.follow(&state(1, "form"));
        assert_eq!(edit.cursor(), 4);
        edit.key(&key("left"));
        edit.follow(&state(1, "form"));
        assert_eq!(edit.cursor(), 3, "an echo keeps the cursor");
        edit.follow(&state(1, "other"));
        assert_eq!(edit.cursor(), 5, "the model changed the text");
        edit.key(&key("home"));
        edit.follow(&state(2, "other"));
        assert_eq!(edit.cursor(), 5, "a field opened afresh");
    }
}
