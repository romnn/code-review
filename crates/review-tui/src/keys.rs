//! Terminal key events as the key matcher takes them, and what has the keyboard.
//!
//! crossterm's key events become [`KeyDown`]s by the rules [`KeyDown`] documents, which are
//! what gpui's Linux platform makes of a key press, so a key does in the terminal what it does
//! in review-gui:
//!
//! - A letter is its lowercase self, with shift when typed as a capital or with shift held:
//!   `G` is `shift-g` typing `G`.
//! - Any other character is itself, never with shift, since shift already chose it: `?` is
//!   `?`, never `shift-/`.
//! - A space is `space` typing a space; named keys keep shift: `shift-tab`, `shift-enter`.
//! - The character a key types goes with it unless Control or Super is held, which makes a
//!   control character or a shortcut, as `ctrl-g` is.
//! - Super becomes the platform modifier; Hyper and Meta, which gpui does not have, are left
//!   out.
//! - Keys that are no keystroke of the grammar, such as Caps Lock or media keys, are dropped,
//!   and so are releases, which only a terminal reporting event types sends.
//!
//! Without the kitty keyboard protocol, a terminal sends some keys as others: `ctrl-h` as
//! Backspace, `ctrl-i` as Tab, `ctrl-m` as Enter and `ctrl-[` as Escape, and shift with
//! control letters not at all.
//! Keys typed in such terminals arrive as what they send.
//! With the protocol's disambiguation, which review-tui asks for where the terminal supports
//! it, each arrives as itself.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use review_ui::keymap::Area;
use review_ui::keys::{KeyDown, Keystroke, Modifiers};
use review_ui::model::{Field, Model};

/// The matcher's key press of a terminal key event, or `None` for a release or a key the
/// grammar has no name for.
#[must_use]
pub fn key_down(event: &KeyEvent) -> Option<KeyDown> {
    if event.kind == KeyEventKind::Release {
        return None;
    }
    let held = event.modifiers;
    let mut modifiers = Modifiers {
        control: held.contains(KeyModifiers::CONTROL),
        alt: held.contains(KeyModifiers::ALT),
        shift: held.contains(KeyModifiers::SHIFT),
        platform: held.contains(KeyModifiers::SUPER),
        function: false,
    };
    let named = |key: &str| Some((key.to_owned(), None));
    let (key, key_char) = match event.code {
        KeyCode::Char(' ') => (String::from("space"), Some(" ".to_owned())),
        KeyCode::Char(character) if character.is_alphabetic() => {
            let lower: String = character.to_lowercase().collect();
            let capital = character.is_uppercase();
            modifiers.shift |= capital;
            let typed = if modifiers.shift {
                character.to_uppercase().collect()
            } else {
                lower.clone()
            };
            (lower, Some(typed))
        }
        KeyCode::Char(character) => {
            // A shifted symbol is the symbol itself.
            modifiers.shift = false;
            (character.to_string(), Some(character.to_string()))
        }
        KeyCode::Enter => named("enter")?,
        KeyCode::Esc => named("escape")?,
        KeyCode::Tab => named("tab")?,
        KeyCode::BackTab => {
            modifiers.shift = true;
            named("tab")?
        }
        KeyCode::Backspace => named("backspace")?,
        KeyCode::Delete => named("delete")?,
        KeyCode::Insert => named("insert")?,
        KeyCode::Up => named("up")?,
        KeyCode::Down => named("down")?,
        KeyCode::Left => named("left")?,
        KeyCode::Right => named("right")?,
        KeyCode::Home => named("home")?,
        KeyCode::End => named("end")?,
        KeyCode::PageUp => named("pageup")?,
        KeyCode::PageDown => named("pagedown")?,
        KeyCode::F(number @ 1..=24) => named(&format!("f{number}"))?,
        _ => return None,
    };
    // Control and Super make a shortcut or a control character, which types nothing.
    let key_char = key_char.filter(|_| !modifiers.control && !modifiers.platform);
    Some(KeyDown {
        keystroke: Keystroke {
            modifiers,
            key,
            key_char,
        },
        prefer_character_input: false,
    })
}

/// The terminal key event a keystroke of the grammar arrives as, for QA mode to type it as
/// a terminal would send it; `None` for a key no terminal sends, such as a lone modifier.
#[must_use]
pub fn key_event(keystroke: &Keystroke) -> Option<KeyEvent> {
    let Modifiers {
        control,
        alt,
        shift,
        platform,
        function: _,
    } = keystroke.modifiers;
    let mut held = KeyModifiers::NONE;
    held.set(KeyModifiers::CONTROL, control);
    held.set(KeyModifiers::ALT, alt);
    held.set(KeyModifiers::SUPER, platform);
    let code = match keystroke.key.as_str() {
        "space" => KeyCode::Char(' '),
        "enter" => KeyCode::Enter,
        "escape" => KeyCode::Esc,
        "tab" if shift => KeyCode::BackTab,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        key => {
            let mut characters = key.chars();
            match (characters.next(), characters.next()) {
                (Some(character), None) if character.is_alphabetic() && shift => {
                    KeyCode::Char(character.to_uppercase().next().unwrap_or(character))
                }
                (Some(character), None) => KeyCode::Char(character),
                _ => KeyCode::F(key.strip_prefix('f')?.parse().ok()?),
            }
        }
    };
    // A terminal reports shift with letters and named keys; a symbol carries it in itself.
    let symbol = matches!(code, KeyCode::Char(character) if !character.is_alphabetic());
    held.set(
        KeyModifiers::SHIFT,
        shift && !symbol && code != KeyCode::BackTab,
    );
    Some(KeyEvent {
        code,
        modifiers: held,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

/// What has the keyboard, which keys waiting for a sequence belong to: a mode, an overlay or
/// a text field opened afresh, as review-gui's focus handles are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The file tree.
    Tree,
    /// The diff.
    Diff,
    /// The file buffer.
    Buffer,
    /// An overlay, a picker's list or the settings list.
    Overlay,
    /// A text field taking typing, by its identity.
    Field(Field, u64),
}

impl Focus {
    /// What has the keyboard in `model`.
    #[must_use]
    pub fn of(model: &Model) -> Self {
        if let Some(field) = model.typing_field()
            && let Some(state) = model
                .fields()
                .into_iter()
                .find(|state| state.field == field)
        {
            return Self::Field(field, state.id);
        }
        match model.area() {
            Area::Tree => Self::Tree,
            Area::Diff => Self::Diff,
            Area::Buffer => Self::Buffer,
            Area::Overlay
            | Area::Picker
            | Area::PickerList
            | Area::SettingsList
            | Area::SettingsSearch
            | Area::Find
            | Area::Window => Self::Overlay,
        }
    }
}

/// The key contexts around the focus, from the window down, as review-gui's element tree has
/// them: the window's, then the area that has the keyboard.
#[must_use]
pub fn contexts(model: &Model) -> [Area; 2] {
    [Area::Window, model.area()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn typed(code: KeyCode, modifiers: KeyModifiers) -> Keystroke {
        key_down(&press(code, modifiers)).unwrap().keystroke
    }

    /// Terminal keys become the keystrokes gpui's Linux platform makes of the same keys.
    #[test]
    fn keys_normalize_as_gpui_types_them() {
        let none = KeyModifiers::NONE;
        let shift = KeyModifiers::SHIFT;
        let control = KeyModifiers::CONTROL;
        let cases = [
            (press(KeyCode::Char('j'), none), "j->j"),
            (press(KeyCode::Char('G'), shift), "shift-g->G"),
            // A terminal without the kitty protocol sends a capital without shift.
            (press(KeyCode::Char('G'), none), "shift-g->G"),
            (press(KeyCode::Char('?'), shift), "?->?"),
            (press(KeyCode::Char('!'), none), "!->!"),
            (press(KeyCode::Char('+'), shift), "+->+"),
            (press(KeyCode::Char('|'), shift), "|->|"),
            (press(KeyCode::Char('0'), none), "0->0"),
            (press(KeyCode::Char(' '), none), "space-> "),
            (press(KeyCode::Char('d'), control), "ctrl-d"),
            (press(KeyCode::Char('='), control), "ctrl-="),
            (press(KeyCode::Char('s'), KeyModifiers::ALT), "alt-s->s"),
            (press(KeyCode::Enter, none), "enter"),
            (press(KeyCode::Esc, none), "escape"),
            (press(KeyCode::BackTab, shift), "shift-tab"),
            (press(KeyCode::Up, none), "up"),
            (press(KeyCode::F(11), none), "f11"),
            (press(KeyCode::Backspace, control), "ctrl-backspace"),
        ];
        for (event, expected) in cases {
            let expected = Keystroke::parse(expected).unwrap();
            assert_eq!(key_down(&event).unwrap().keystroke, expected, "{event:?}");
        }
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..press(KeyCode::Char(' '), none)
        };
        assert_eq!(key_down(&release), None, "releases are never key presses");
        assert_eq!(key_down(&press(KeyCode::CapsLock, none)), None);
    }

    /// Every keystroke the keymap binds that a terminal can send comes back as itself after a
    /// round through the event QA mode types for it, so QA mode exercises the normalization.
    #[test]
    fn keystrokes_round_trip_through_terminal_events() {
        let (bindings, _) = review_ui::keymap::keymap(&std::collections::BTreeMap::new());
        let mut checked = 0;
        for keys in bindings.iter().flat_map(|binding| binding.keys.iter()) {
            for source in keys.split_whitespace() {
                let bound = Keystroke::parse(source).unwrap();
                // A terminal sends a shifted symbol as the symbol itself, such as `+` for
                // `ctrl-shift-=`, which the keymap binds as well.
                let symbol =
                    !bound.key.chars().all(char::is_alphabetic) && bound.key.chars().count() == 1;
                if bound.modifiers.shift && symbol {
                    continue;
                }
                let Some(event) = key_event(&bound) else {
                    continue;
                };
                let typed = key_down(&event).unwrap().keystroke;
                assert!(typed.matches(&bound), "{source}: {typed:?}");
                checked += 1;
            }
        }
        assert!(checked > 100, "{checked} keystrokes");
        assert_eq!(
            typed(KeyCode::Char('Z'), KeyModifiers::SHIFT),
            Keystroke::parse("shift-z->Z").unwrap()
        );
    }
}
