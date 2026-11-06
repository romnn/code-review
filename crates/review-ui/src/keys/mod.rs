//! Keystrokes as the keymap and the `[keys]` overrides write them, such as `ctrl-shift-e` or the
//! sequence `g g`, key presses as the front ends type them, and the [`Matcher`] that turns key
//! presses into the keymap's actions.
//!
//! All of it is gpui's, ported from gpui-pre 0.3.9: the grammar from `Keystroke::parse` and
//! the matching of a typed keystroke from `Keystroke::should_match`
//! (`src/platform/keystroke.rs`), and the matcher from gpui's key dispatch (see [`Matcher`]).
//! So a key does the same in every front end, and the same as it did when review-gui bound
//! its keys in gpui; review-gui's tests run gpui and the port over the same keys.

mod matcher;
#[cfg(test)]
mod tests;

pub use matcher::{Matcher, Matches, Outcome, PENDING_TIMEOUT, Replay, Resolution, Target};

/// The modifier keys held with a key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    /// The control key.
    pub control: bool,
    /// The alt key, also known as meta or option.
    pub alt: bool,
    /// The shift key.
    pub shift: bool,
    /// The command key on macOS, the Windows key on Windows and the super key on Linux.
    pub platform: bool,
    /// The function key.
    pub function: bool,
}

/// One parsed keystroke: modifiers and a key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Keystroke {
    /// The modifiers held.
    pub modifiers: Modifiers,
    /// The key, lowercase, such as `e`, `-`, `space` or `f11`; a lone modifier such as `shift`
    /// is the key itself, named `shift`, `control`, `alt`, `platform` or `function`.
    pub key: String,
    /// The character the keystroke types, given only by the `key->char` form test events use.
    pub key_char: Option<String>,
}

/// A keystroke that does not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid keystroke {keystroke:?}: expected modifiers (`ctrl`, `alt`, `shift`, `fn`, `cmd`, \
     `super` or `win`) followed by a key, separated by `-`"
)]
pub struct InvalidKeystroke {
    /// The text that does not parse.
    pub keystroke: String,
}

impl Keystroke {
    /// Parses one keystroke: `[modifier-]…key[->key_char]`.
    ///
    /// Modifiers are `ctrl`, `alt`, `shift`, `fn`, `cmd`, `super`, `win` and `secondary` (`cmd`
    /// on macOS, `ctrl` elsewhere), in any case and order.
    /// A single uppercase ASCII letter means shift and that letter; other keys are lowercased.
    /// A trailing `-` is the minus key, as in `ctrl--`.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidKeystroke`] when a key is followed by more than a trailing `-` or a
    /// `->key_char`, or when there is no key at all.
    pub fn parse(source: &str) -> Result<Self, InvalidKeystroke> {
        let invalid = || InvalidKeystroke {
            keystroke: source.to_owned(),
        };
        let mut modifiers = Modifiers::default();
        let mut key = None;
        let mut key_char = None;

        let mut components = source.split('-').peekable();
        while let Some(component) = components.next() {
            // Modifiers
            if component.eq_ignore_ascii_case("ctrl") {
                modifiers.control = true;
                continue;
            }
            if component.eq_ignore_ascii_case("alt") {
                modifiers.alt = true;
                continue;
            }
            if component.eq_ignore_ascii_case("shift") {
                modifiers.shift = true;
                continue;
            }
            if component.eq_ignore_ascii_case("fn") {
                modifiers.function = true;
                continue;
            }
            if component.eq_ignore_ascii_case("secondary") {
                if cfg!(target_os = "macos") {
                    modifiers.platform = true;
                } else {
                    modifiers.control = true;
                }
                continue;
            }
            if component.eq_ignore_ascii_case("cmd")
                || component.eq_ignore_ascii_case("super")
                || component.eq_ignore_ascii_case("win")
            {
                modifiers.platform = true;
                continue;
            }

            // A key with something after it: the minus key, a `->key_char`, or an error.
            // As in gpui, parsing goes on after a `->key_char`, so a later component replaces
            // the key.
            if let Some(next) = components.peek() {
                if next.is_empty() && source.ends_with('-') {
                    key = Some("-".to_owned());
                    break;
                }
                match next.strip_prefix('>').filter(|typed| !typed.is_empty()) {
                    Some(typed) => {
                        key = Some(component.to_owned());
                        key_char = Some(typed.to_owned());
                        components.next();
                    }
                    None => return Err(invalid()),
                }
                continue;
            }

            // The last key: an uppercase letter is shift and the letter, and named keys are
            // matched in lowercase.
            if component.len() == 1 && component.bytes().all(|byte| byte.is_ascii_uppercase()) {
                modifiers.shift = true;
            }
            key = Some(component.to_ascii_lowercase());
        }

        // A lone modifier is the key itself, and no longer a modifier.
        let key = key.or_else(|| {
            if std::mem::take(&mut modifiers.shift) {
                Some("shift".to_owned())
            } else if std::mem::take(&mut modifiers.control) {
                Some("control".to_owned())
            } else if std::mem::take(&mut modifiers.alt) {
                Some("alt".to_owned())
            } else if std::mem::take(&mut modifiers.platform) {
                Some("platform".to_owned())
            } else if std::mem::take(&mut modifiers.function) {
                Some("function".to_owned())
            } else {
                None
            }
        });
        Ok(Self {
            modifiers,
            key: key.ok_or_else(invalid)?,
            key_char,
        })
    }

    /// Parses a keystroke sequence, keystrokes separated by whitespace, such as `g g` or
    /// `z shift-m`; blank text is the empty sequence.
    ///
    /// # Errors
    ///
    /// Returns the first keystroke that does not parse.
    pub fn parse_sequence(source: &str) -> Result<Vec<Self>, InvalidKeystroke> {
        source.split_whitespace().map(Self::parse).collect()
    }

    /// Whether this typed keystroke triggers `bound`, a keystroke of a binding.
    ///
    /// It does when both have the same modifiers and key.
    /// It also does when the typed keystroke types a character other than its key and `bound`
    /// is that character with the typed control and platform modifiers and no others: a layout
    /// that types `ß` with `alt-s` triggers a binding of `ß`, and `shift-/` typing `?` triggers
    /// `?`.
    /// On Windows, as in gpui, only a `bound` without modifiers matches the typed character.
    #[must_use]
    pub fn matches(&self, bound: &Self) -> bool {
        if let Some(key_char) = self.key_char.as_ref().filter(|typed| **typed != self.key) {
            let character_modifiers = if cfg!(target_os = "windows") {
                Modifiers::default()
            } else {
                Modifiers {
                    control: self.modifiers.control,
                    platform: self.modifiers.platform,
                    ..Modifiers::default()
                }
            };
            if bound.key == *key_char && bound.modifiers == character_modifiers {
                return true;
            }
        }
        bound.modifiers == self.modifiers && bound.key == self.key
    }
}

/// A key press, as each front end normalizes its platform's key events for the [`Matcher`].
///
/// From gpui's `KeyDownEvent`, `keystroke.modifiers` (control, alt, shift, platform and
/// function), `keystroke.key` and `keystroke.key_char` become [`Self::keystroke`] field for
/// field, and `prefer_character_input` becomes [`Self::prefer_character_input`]; `is_held` is
/// left out, because a held key's repeats are key presses of their own.
///
/// Other platforms' key events follow what gpui's Linux platform makes of a key press:
///
/// - The key is named as the grammar names it: a lowercase letter, a digit, the symbol itself
///   (`?`, `!`, `+`, `_`, `|`, `=`), or `space`, `enter`, `escape`, `tab`, `backspace`,
///   `delete`, `insert`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`
///   and `f1` to `f24`.
/// - Shift is held for letters and named keys only: a shifted symbol is the symbol, so `?` is
///   typed as `?`, never as `shift-/`, while `G` is `shift-g`.
/// - The typed character is the text the key types, such as `G` for `shift-g` or a space for
///   `space`, and `None` when it types none or a control character, as `ctrl-g`, `enter` and
///   `escape` do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDown {
    /// The modifiers, the key and the character it types.
    pub keystroke: Keystroke,
    /// Whether the platform would rather the key type its character into a text field than
    /// trigger a binding, as macOS has it for characters typed with option; gpui's Linux
    /// platform never does.
    pub prefer_character_input: bool,
}
