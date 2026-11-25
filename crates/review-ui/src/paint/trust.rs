//! The question whether to trust the checkout, asked before a tool runs its code in it.

use super::{Palette, Piece, Weight};
use crate::model::{Model, Overlay};

/// The trust question, as a card over the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustPrompt {
    /// The question, semibold.
    pub title: Piece,
    /// Why it is asked, a paragraph a line.
    pub body: Vec<Piece>,
    /// The answers, each a key and what it does.
    pub choices: Vec<Choice>,
}

/// One answer of the trust question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The key, in the accent.
    pub key: Piece,
    /// What it does.
    pub label: Piece,
}

/// The trust question, while it shows.
#[must_use]
pub fn trust_prompt(model: &Model, palette: &Palette) -> Option<TrustPrompt> {
    if model.overlay != Some(Overlay::Trust) {
        return None;
    }
    let choice = |key: &str, label: &str| Choice {
        key: Piece::new(key, palette.accent).weighted(Weight::Semibold),
        label: Piece::new(label, palette.text),
    };
    Some(TrustPrompt {
        title: Piece::new("Trust this checkout?", palette.text).weighted(Weight::Semibold),
        body: vec![
            Piece::new(
                "Go to definition starts a language server in this checkout with its builds \
                 on: build scripts, proc macros and the workspace's configuration can run any \
                 program and write in the checkout.",
                palette.dim,
            ),
            Piece::new(
                "The grant covers later edits and branch switches in this clone, until Ctrl+T \
                 revokes it. Trust only code you would build and run yourself.",
                palette.dim,
            ),
        ],
        choices: vec![
            choice("c", "trust this checkout, until Ctrl+T revokes it"),
            choice("Esc", "not now"),
        ],
    })
}
