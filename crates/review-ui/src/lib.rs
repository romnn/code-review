//! The review front ends' shared model, apart from drawing and platform input.
//!
//! review-gui draws it with gpui and review-tui with ratatui in a terminal, so everything here
//! stays free of any UI toolkit.
//!
//! The heart is [`model::Model`], the whole review window apart from its drawing: the snapshot
//! on screen, the selected file and hunk, marks waiting for the engine, folds, find, pickers,
//! settings and the file buffer.
//! A front end drives it through a [`model::Driver`], which hands back the [`model::Effect`]s
//! the model queues (background jobs, timers, the clipboard, the window title) for the front
//! end to run; a job's or a timer's [`model::Completion`] goes back to the model, which lands it
//! only while what it was computed for still shows.
//! Each frame, the front end measures a [`frame::Geometry`], plain data, and hands it to the
//! model's layout phase, which resolves scrolling against the real viewports; the model owns
//! every list's offset in rows and works out its rows and cells from the last geometry.
//!
//! Key presses go through one [`keys::Matcher`], which resolves them into the model's actions
//! as gpui's key dispatch resolves key bindings, so keys work the same in every front end.
//!
//! What the window draws and in which colors, apart from geometry, is [`paint`]'s: every front
//! end draws its pieces, so the same review looks the same in each.
//!
//! Both front ends start the same way ([`launch`]), tell the model what they can do
//! ([`front_end`]), edit text fields with the same keys ([`line_edit`] in a terminal, gpui's
//! input in a window) and answer QA mode's `state` alike ([`qa`]).
//!
//! The rest is what the model is built from: the configuration file, themes and diff styles,
//! the rows a diff is drawn from, syntax highlighting, the keymap and its keystroke grammar,
//! find and selection over rows, scroll arithmetic, the review snapshot on screen, and the
//! language server client.
//!
//! The crate is GPL-3.0-or-later, like the apps it is the model of.

pub mod color;
pub mod config;
pub mod find;
pub mod folders;
pub mod frame;
pub mod front_end;
pub mod keymap;
pub mod keys;
pub mod launch;
pub mod line_edit;
pub mod look;
pub mod lsp;
pub mod markdown;
pub mod model;
pub mod paint;
pub mod panes;
pub mod qa;
pub mod rows;
pub mod scroll;
pub mod state;
pub mod syntax;
pub mod text;
pub mod theme;
