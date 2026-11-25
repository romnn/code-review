//! The whole file as a read-only vim buffer: the head version with change markers in the
//! gutter, vim motions, find, and go-to-definition into other files with a jump back.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use review_core::{RevisionView, UnitId};

use super::effects::{Outcome, Subject};
use super::engine::{Servers, Ticket};
use super::{DEFINITION_DEADLINE, Mode, Model, Trust};
use crate::frame::Request;
use crate::lsp::{self, LanguageServer};
use crate::paint::display::DisplayLine;
use crate::rows::FullText;
use crate::scroll;
use crate::syntax::SideSyntax;
use crate::text::{self, TextPos};

/// The cells kept beside the buffer's cursor when its code pans to follow it.
const COLUMN_AIR: f64 = 4.0;

/// What a line of a review buffer changed, for its gutter marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// The line was added or changed by this unit.
    Added(UnitId),
    /// Lines were removed just before this line by this unit.
    Removed(UnitId),
}

impl Marker {
    /// The unit that changed the line. Markers name units, not display hunks, since which
    /// hunk holds a unit depends on the view the diff shows.
    #[must_use]
    pub fn unit(self) -> UnitId {
        match self {
            Self::Added(unit) | Self::Removed(unit) => unit,
        }
    }
}

/// Where a buffer's text comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The head version of a file under review, by display path.
    Review(String),
    /// A file a definition led to, by its path in the worktree.
    Definition(String),
    /// A file the review did not change, by its path in the head snapshot, opened from a
    /// picker.
    Head(String),
}

/// One file shown in full.
#[derive(Clone)]
pub struct Buffer {
    /// Unique for the session, so an answer for one buffer is never applied to another.
    pub(crate) id: u64,
    /// Where the text comes from, which also names the buffer.
    pub origin: Origin,
    /// The text, one entry per line.
    pub lines: Arc<FullText>,
    /// The text's highlights, once they are computed; `None` until then, or for a language
    /// without a grammar.
    pub syntax: Option<Arc<SideSyntax>>,
    /// Each line's change marker, for a review buffer.
    pub markers: Arc<Vec<Option<Marker>>>,
    /// The cursor, which the buffer's motions and go-to-definition start from.
    pub cursor: TextPos,
    /// The buffer a definition jump left, for `Ctrl+O`.
    pub previous: Option<Box<Buffer>>,
}

impl Buffer {
    /// The title the buffer's header shows.
    #[must_use]
    pub fn title(&self) -> &str {
        match &self.origin {
            Origin::Review(path) | Origin::Definition(path) | Origin::Head(path) => path,
        }
    }

    /// The breadcrumb of the scopes around the cursor, such as `impl Ledger › fn balance`,
    /// counting a scope that starts on the cursor's line.
    #[must_use]
    pub fn scope(&self) -> Option<String> {
        let row = self.cursor.row;
        let scope = self
            .syntax
            .as_ref()?
            .scopes
            .scopes()
            .iter()
            .filter(|scope| scope.start_line <= row && row <= scope.end_line)
            .map(|scope| scope.label.as_str())
            .collect::<Vec<_>>()
            .join(" › ");
        (!scope.is_empty()).then_some(scope)
    }

    /// The text of line `row`.
    pub fn line(&self, row: usize) -> &str {
        self.lines.lines.get(row).map_or("", String::as_str)
    }
}

impl Model {
    /// The display hunk of the line under the buffer cursor, only in a review buffer of the
    /// selected file: the hunk of the diff's rows now that holds the line's unit, or for a unit
    /// that shows its changes since it was reviewed as hunks of their own, the one the line is
    /// in, and none on a line the same as reviewed.
    #[must_use]
    pub fn buffer_hunk(&self) -> Option<usize> {
        let buffer = self.buffer.as_ref()?;
        if buffer.origin != Origin::Review(self.selected.clone()?) {
            return None;
        }
        let row = buffer.cursor.row;
        let unit = buffer.markers.get(row).copied().flatten()?.unit();
        let last = buffer.markers.len().saturating_sub(1);
        self.rows.hunks.iter().position(|hunk| {
            hunk.unit == unit
                && hunk.lines().is_none_or(|lines| {
                    // A change that only removes lines stands at the line after them.
                    lines.contains(&row) || (lines.is_empty() && lines.start.min(last) == row)
                })
        })
    }

    /// The first display hunk of the shown rows that holds `unit`.
    #[must_use]
    pub(crate) fn hunk_of(&self, unit: UnitId) -> Option<usize> {
        self.rows.hunk_of(unit, None)
    }

    /// `=`: the whole file at the selected hunk, or back to the diff at the hunk under the
    /// cursor.
    pub(super) fn toggle_buffer(&mut self) {
        if self.mode == Mode::Buffer {
            self.leave_buffer();
        } else {
            self.enter_buffer();
        }
    }

    /// Opens the selected file as a buffer, with the cursor on the selected hunk.
    pub(super) fn enter_buffer(&mut self) {
        let Some(path) = self.selected.clone() else {
            return;
        };
        let Some(view) = self.view() else {
            return;
        };
        let units = view.units_of_file(&path);
        let Some(head) = units
            .first()
            .and_then(|&unit| view.file(unit).ok())
            .and_then(|file| file.new.clone())
        else {
            self.set_status("This file was deleted; only its diff shows it.");
            return;
        };
        let Some(lines) = self.blobs.texts.get(&head.oid).cloned() else {
            self.set_status("This file is binary or not read yet.");
            return;
        };
        let markers = self.buffer_markers(&view, lines.lines.len());
        let start_row = self
            .rows
            .hunks
            .get(self.hunk)
            .and_then(|hunk| match hunk.at() {
                Some(start) => Some(start),
                None => Some(view.unit(hunk.unit).ok()?.chunk()?.new.start),
            });
        let row = start_row
            .unwrap_or(0)
            .min(lines.lines.len().saturating_sub(1));
        let buffer = Buffer {
            id: self.next_buffer_id(),
            origin: Origin::Review(path),
            syntax: self.blobs.side_syntax(&head),
            lines,
            markers: Arc::new(markers),
            cursor: TextPos { row, col: 0 },
            previous: None,
        };
        self.set_buffer(Some(buffer));
        self.buffer_scroll.stop();
        // The list may not be laid out yet, so the cursor is centered once it is.
        self.buffer_scroll.request(Request::Center(row));
        self.enter_mode(Mode::Buffer);
    }

    /// The change markers of a review buffer of the shown file, `len` lines long: each display
    /// hunk's lines, so a unit that shows its changes since review apart marks only those, as
    /// the review keys act on them.
    fn buffer_markers(&self, view: &RevisionView<'_>, len: usize) -> Vec<Option<Marker>> {
        let mut markers = vec![None; len.max(1)];
        let last = markers.len().saturating_sub(1);
        for hunk in &self.rows.hunks {
            let unit = hunk.unit;
            let lines = match hunk.lines() {
                Some(lines) => lines,
                None => match view.unit(unit).ok().and_then(|record| record.chunk()) {
                    Some(chunk) => chunk.new.range(),
                    None => continue,
                },
            };
            if lines.is_empty()
                && let Some(slot) = markers.get_mut(lines.start.min(last))
            {
                slot.get_or_insert(Marker::Removed(unit));
            }
            for row in lines {
                if let Some(slot) = markers.get_mut(row) {
                    *slot = Some(Marker::Added(unit));
                }
            }
        }
        markers
    }

    /// Marks again the lines of the review buffer of the shown file after its rows changed,
    /// such as when one change of a unit was approved.
    pub(super) fn remark_buffer(&mut self) {
        let markers = {
            let (Some(view), Some(buffer), Some(path)) =
                (self.view(), &self.buffer, &self.selected)
            else {
                return;
            };
            if buffer.origin != Origin::Review(path.clone()) {
                return;
            }
            self.buffer_markers(&view, buffer.lines.lines.len())
        };
        if let Some(buffer) = &mut self.buffer {
            buffer.markers = Arc::new(markers);
        }
    }

    /// A fresh buffer identity.
    pub(super) fn next_buffer_id(&mut self) -> u64 {
        self.buffer_ids += 1;
        self.buffer_ids
    }

    /// Shows `next` instead of the current buffer. The search belongs to the buffer's text, so
    /// its matches are found again in the new text, or dropped with the buffer.
    pub(super) fn set_buffer(&mut self, next: Option<Buffer>) {
        self.buffer = next;
        self.selection = None;
        // Another text starts at its left edge.
        self.buffer_text.reset();
        self.buffer_column_waits = false;
        if self.buffer.is_none()
            && self
                .find
                .as_ref()
                .is_some_and(|find| find.target == Mode::Buffer)
        {
            self.find = None;
        }
        self.refresh_find(Mode::Buffer);
    }

    /// Back to the diff, at the hunk the cursor is in or the last one before it.
    pub(super) fn leave_buffer(&mut self) {
        let row = self.buffer.as_ref().map(|buffer| buffer.cursor.row);
        let hunk = self.buffer_hunk().or_else(|| {
            let row = row?;
            let buffer = self.buffer.as_ref()?;
            buffer
                .markers
                .iter()
                .take(row + 1)
                .rev()
                .find_map(|marker| self.hunk_of(marker.as_ref()?.unit()))
        });
        self.find = None;
        self.enter_mode(Mode::Diff);
        if let Some(hunk) = hunk {
            self.select_hunk(hunk);
        } else {
            self.reveal_hunk();
        }
    }

    /// Esc in the buffer: closes the find results, then returns from a definition, then goes
    /// back to the diff.
    pub(super) fn buffer_dismiss(&mut self) {
        if self.find.take().is_some() {
            return;
        }
        if self
            .selection
            .take()
            .is_some_and(|selection| !selection.is_empty())
        {
            return;
        }
        if self
            .buffer
            .as_ref()
            .is_some_and(|buffer| buffer.previous.is_some())
        {
            self.jump_back();
            return;
        }
        self.leave_buffer();
    }

    /// Moves the cursor to `row`, keeping the column within the line, and scrolls to it.
    pub(super) fn set_buffer_cursor(&mut self, row: usize, col: usize) {
        let Some(buffer) = &mut self.buffer else {
            return;
        };
        let row = row.min(buffer.lines.lines.len().saturating_sub(1));
        let col = col.min(buffer.line(row).len());
        buffer.cursor = TextPos {
            row,
            col: text::floor_char_boundary(buffer.line(row), col),
        };
        self.reveal_buffer_cursor();
    }

    /// Moves the cursor by `rows` lines.
    pub(super) fn move_buffer_cursor_by(&mut self, rows: isize) {
        let Some(cursor) = self.buffer.as_ref().map(|buffer| buffer.cursor) else {
            return;
        };
        self.set_buffer_cursor(cursor.row.saturating_add_signed(rows), cursor.col);
    }

    /// Scrolls the buffer so its cursor has some lines of air, and pans its code so the
    /// cursor has some cells beside it.
    fn reveal_buffer_cursor(&mut self) {
        let Some(row) = self.buffer.as_ref().map(|buffer| buffer.cursor.row) else {
            return;
        };
        self.reveal_buffer_column();
        // A list not laid out yet has no height to reveal the cursor in, so it is centered
        // once it has one, as a newly opened buffer's is.
        let Some(viewport) = self.grid().viewports.buffer else {
            self.buffer_scroll.stop();
            self.buffer_scroll.request(Request::Center(row));
            return;
        };
        let top = scroll::rows(row);
        if let Some(target) =
            scroll::reveal(self.buffer_scroll.target(), viewport, top, top + 1.0, 4.0)
        {
            self.ease_buffer_to(target);
        }
    }

    /// Pans the buffer's code so its cursor has some cells beside it, once the buffer list has
    /// a width: before its first layout the room is unknown, and revealing the cursor in no
    /// room would move it and the start of every line out of view, so the layout phase that
    /// first has the width reveals it instead.
    pub(super) fn reveal_buffer_column(&mut self) {
        self.measure_buffer_text();
        self.buffer_column_waits = !self.geometry.buffer_measured();
        if self.buffer_column_waits {
            return;
        }
        let Some(cell) = self.buffer_cursor_cell() else {
            return;
        };
        let room = self.buffer_room();
        self.buffer_text.reveal(cell, room, COLUMN_AIR);
    }

    /// The cell the buffer's cursor shows in on its line.
    fn buffer_cursor_cell(&self) -> Option<usize> {
        let buffer = self.buffer.as_ref()?;
        Some(DisplayLine::new(buffer.line(buffer.cursor.row)).cell_of(buffer.cursor.col))
    }

    /// Whether the buffer's cursor shows in `room` cells at the code's offset now.
    pub(super) fn buffer_cursor_shown(&self, room: f64) -> bool {
        self.buffer_cursor_cell()
            .is_some_and(|cell| self.buffer_text.shows(cell, room))
    }

    /// `w` and `b`: the next or previous identifier, across lines.
    pub(super) fn move_word(&mut self, forward: bool) {
        let Some(buffer) = &self.buffer else {
            return;
        };
        let TextPos { row, col } = buffer.cursor;
        let count = buffer.lines.lines.len();
        let found = if forward {
            (row..count).find_map(|line| {
                text::identifiers(buffer.line(line))
                    .into_iter()
                    .find(|word| line > row || word.start > col)
                    .map(|word| (line, word.start))
            })
        } else {
            (0..=row).rev().find_map(|line| {
                text::identifiers(buffer.line(line))
                    .into_iter()
                    .rev()
                    .find(|word| line < row || word.start < col)
                    .map(|word| (line, word.start))
            })
        };
        if let Some((row, col)) = found {
            self.set_buffer_cursor(row, col);
        }
    }

    /// Updates the review buffer's highlights once they are computed.
    pub(super) fn refresh_buffer_syntax(&mut self) {
        let Some(Origin::Review(path)) = self.buffer.as_ref().map(|buffer| buffer.origin.clone())
        else {
            return;
        };
        let syntax = self
            .highlights
            .head_at(&path)
            .and_then(|side| self.blobs.syntax_of(side));
        if let Some(buffer) = &mut self.buffer {
            buffer.syntax = syntax;
        }
    }

    /// `g d`: asks a language server, in the background, where the identifier under the cursor
    /// is defined, and moves there, opening another file as a buffer when it is elsewhere.
    pub(super) fn go_to_definition(&mut self) {
        self.trust_pending = None;
        if !self.settings.language_servers {
            self.set_status(
                "Go to definition is off: language servers never start while [lsp] enabled = \
                 false in config.toml.",
            );
            return;
        }
        match self.trust {
            Trust::Granted(_) => {}
            Trust::Checking => {
                self.set_status("Trust is being checked; press g d again in a moment.");
                return;
            }
            Trust::Denied if self.is_checkout() => {
                self.ask_trust(true);
                return;
            }
            Trust::Denied => {
                self.set_status(
                    "Go to definition needs a language server, which runs code from this \
                     review. Press Ctrl+T to trust it first.",
                );
                return;
            }
        }
        if self.looking_up {
            self.set_status("A definition lookup is still running.");
            return;
        }
        let Some(buffer) = &self.buffer else {
            return;
        };
        let path = match &buffer.origin {
            Origin::Review(path) | Origin::Definition(path) | Origin::Head(path) => path.clone(),
        };
        let (row, col) = (buffer.cursor.row, buffer.cursor.col);
        if text::identifiers(buffer.line(row))
            .iter()
            .all(|word| !(word.start <= col && col < word.end))
        {
            self.set_status("Put the cursor on a name to go to its definition.");
            return;
        }
        let Some(head) = self.shown_head().filter(|_| self.engine.is_some()) else {
            return;
        };
        // The answer lands only in the buffer, snapshot and trust epoch it was asked in.
        let ticket = self.ticket();
        let lookup = Lookup {
            buffer: buffer.id,
            shown: self.shown,
            epoch: ticket.epoch,
        };
        let current = Arc::clone(&self.trust_epoch);
        self.looking_up = true;
        self.set_status("Looking up the definition; the language server may need to index first…");
        self.spawn_with(Subject::Lookup(lookup), move |parts| {
            let review = &parts.review;
            let found = review
                .repo()
                .tree_of(&head)
                .and_then(|tree| {
                    let root = review.tool_worktree(&tree)?;
                    Ok((tree, root))
                })
                .map_err(|err| match err {
                    // Trust found denied goes the way a reading that finds it does.
                    review_core::Error::Untrusted(_) => {
                        parts.servers.check(ticket, &current, || Trust::Denied);
                        Unanswered::Denied
                    }
                    err => Unanswered::Failed(err.to_string()),
                })
                .and_then(|(tree, root)| {
                    let asked = Asked {
                        servers: &parts.servers,
                        ticket,
                        current: &current,
                        trusted: &|| review.trust_for(&tree).is_ok_and(|trust| trust.trusted),
                    };
                    find_definition(&asked, &root, &path, row, col)
                });
            Outcome::Definition(found)
        });
    }

    /// Takes in the answer of a definition lookup for the buffer, revision and trust epoch that
    /// show; trust found denied meanwhile shows as denied.
    pub(super) fn definition_found(&mut self, found: Result<Definition, Unanswered>) {
        match found {
            Ok(found) => self.show_definition(found),
            Err(Unanswered::Denied) => {
                self.take_trust(Trust::Denied);
                self.fail(
                    "Go to definition: trust in this code no longer applies, as when another \
                     window revoked it, so its language servers stopped.",
                );
            }
            Err(Unanswered::Stale) => self.fail(format!("Go to definition: {REVOKED_TRUST}")),
            Err(Unanswered::Failed(err)) => self.fail(format!("Go to definition: {err}")),
        }
    }

    /// Moves to a definition: in this buffer when it is in the same file, otherwise in a buffer
    /// of the other file that `Ctrl+O` leaves.
    fn show_definition(&mut self, found: Definition) {
        let Some(current) = self.buffer.take() else {
            return;
        };
        let same = current.title() == found.path;
        let mut next = if same {
            current
        } else {
            Buffer {
                id: self.next_buffer_id(),
                origin: Origin::Definition(found.path.clone()),
                syntax: Some(Arc::new(found.syntax)),
                lines: Arc::new(found.lines),
                markers: Arc::new(Vec::new()),
                cursor: TextPos { row: 0, col: 0 },
                previous: Some(Box::new(current)),
            }
        };
        next.cursor = TextPos {
            row: found.line,
            col: found.column,
        };
        if same {
            self.buffer = Some(next);
            self.selection = None;
        } else {
            self.set_buffer(Some(next));
        }
        self.reveal_buffer_cursor();
        self.set_status(format!("{}:{}", found.path, found.line + 1));
    }

    /// `Ctrl+O`: back to the buffer a definition jump left.
    pub(super) fn jump_back(&mut self) {
        let previous = self
            .buffer
            .as_mut()
            .and_then(|buffer| buffer.previous.take());
        if let Some(previous) = previous {
            self.set_buffer(Some(*previous));
            self.reveal_buffer_cursor();
        }
    }
}

/// What a definition lookup was asked for, so its answer lands only there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Lookup {
    /// The buffer the cursor was in.
    pub(super) buffer: u64,
    /// The generation of the snapshot shown; see [`Model::shown`].
    pub(super) shown: u64,
    /// The trust epoch it was asked in, which a revocation, even one granted again since, has
    /// moved on from; see the `trust` module.
    pub(super) epoch: u64,
}

/// Where a definition is, with its file's text.
pub(super) struct Definition {
    /// The file, relative to the worktree.
    pub(super) path: String,
    /// The definition's zero-based line.
    pub(super) line: usize,
    /// Its byte column in the line.
    pub(super) column: usize,
    /// The file's text.
    pub(super) lines: FullText,
    /// The file's highlights.
    pub(super) syntax: SideSyntax,
}

/// Why a definition lookup stopped: trust was revoked, or another snapshot shows, while it ran.
const REVOKED_TRUST: &str = "trust was revoked or another snapshot shows, so no language server \
                             runs";

/// Why a definition lookup came back without a definition.
#[derive(Debug)]
pub(super) enum Unanswered {
    /// Its ticket no longer counts: trust was decided, or another head shows, so the window
    /// moved its trust epoch on.
    Stale,
    /// Trust was found denied since the lookup started, and every language server stopped.
    Denied,
    /// Anything else, as the status line says it.
    Failed(String),
}

/// What a definition lookup asks the language servers with.
struct Asked<'a> {
    servers: &'a Servers,
    ticket: Ticket,
    /// The window's trust epoch now.
    current: &'a AtomicU64,
    /// Whether the stored grant still applies.
    trusted: &'a dyn Fn() -> bool,
}

/// Finds where the identifier at byte `column` of line `line` of `path` (relative to `root`) is
/// defined, starting a language server for `root` on first use while the servers admit the
/// lookup (see [`Servers::admit`]), and reads that file.
///
/// The handshake and the request run outside the servers' lock, with the server in the set,
/// so a revocation meanwhile stops it there and the wait fails.
/// A stored grant gone once the server answered, as when another window revoked it meanwhile,
/// stops the servers as a lookup that finds it gone before does, and the answer is dropped.
fn find_definition(
    asked: &Asked<'_>,
    root: &Path,
    path: &str,
    line: usize,
    column: usize,
) -> Result<Definition, Unanswered> {
    let failed = Unanswered::Failed;
    // Language servers answer with canonical paths, so the root must be canonical to strip it.
    let root = &root
        .canonicalize()
        .map_err(|err| failed(format!("cannot open {}: {err}", root.display())))?;
    let file = root.join(path);
    let command = lsp::server_for(&file)
        .ok_or_else(|| failed(format!("no language server is configured for {path}")))?;
    let text = std::fs::read_to_string(&file)
        .map_err(|err| failed(format!("cannot read {path}: {err}")))?;
    let source = text
        .lines()
        .nth(line)
        .ok_or_else(|| failed(format!("{path} has no line {}", line + 1)))?;
    let position = lsp::Position {
        line,
        character: lsp::utf16_column(source, column.min(source.len())),
    };
    let server = asked.servers.admit(
        asked.ticket,
        asked.current,
        asked.trusted,
        command.program,
        || LanguageServer::launch(&command, root).map_err(|err| err.to_string()),
    )?;
    let initialized = server.lock().initialize(&command, root);
    let answered = match initialized {
        Ok(()) => server
            .lock()
            .definition(&file, &text, position, DEFINITION_DEADLINE),
        Err(err) => {
            asked.servers.forget(command.program, &server);
            Err(err)
        }
    };
    drop(server);
    if !(asked.trusted)() {
        asked
            .servers
            .check(asked.ticket, asked.current, || Trust::Denied);
        return Err(Unanswered::Denied);
    }
    let definition = answered.map_err(|err| failed(err.to_string()))?;
    let target = std::fs::read_to_string(&definition.path)
        .map_err(|err| failed(format!("cannot read {}: {err}", definition.path.display())))?;
    // In the root's canonical spelling, which the server's answer need not share: Windows
    // canonical paths carry a `\\?\` prefix that no URI does.
    let target_path = definition
        .path
        .canonicalize()
        .unwrap_or_else(|_| definition.path.clone());
    // Inside the review, the path as the review names files, with `/` on every platform.
    let shown = target_path.strip_prefix(root).map_or_else(
        |_| target_path.display().to_string(),
        |relative| {
            relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        },
    );
    let target_line = target.lines().nth(definition.position.line).unwrap_or("");
    let column = lsp::byte_column(target_line, definition.position.character);
    Ok(Definition {
        syntax: crate::syntax::analyze(&shown, &target),
        lines: FullText::new(&target),
        path: shown,
        line: definition.position.line,
        column,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a lookup of the window's first showing asks with, the stored grant applying as
    /// `trusted` says.
    fn asked<'a>(servers: &'a Servers, trusted: &'a dyn Fn() -> bool) -> Asked<'a> {
        static CURRENT: AtomicU64 = AtomicU64::new(0);
        Asked {
            servers,
            ticket: Ticket {
                showing: 0,
                epoch: 0,
            },
            current: &CURRENT,
            trusted,
        }
    }

    /// Follows a method call in the demo to its definition; skipped where rust-analyzer is not
    /// installed.
    #[test]
    fn definition_from_a_demo_head_line() {
        if !crate::lsp::rust_analyzer_installed() {
            eprintln!("skipped: rust-analyzer is not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let review = review_core::demo::prepare(
            &review_core::demo::DemoPaths::under(dir.path()),
            &review_core::demo::fixture_dir(),
            review_core::demo::DemoRequest::Fresh,
        )
        .unwrap();
        let history = review.history().unwrap();
        let root = review
            .worktree(history.revisions().last().unwrap())
            .unwrap();
        let text = std::fs::read_to_string(root.join("src/engine.rs")).unwrap();
        let (line, source) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("self.balance(account)"))
            .unwrap();
        let column = source.find("balance(").unwrap();
        let servers = Servers::default();
        let found = find_definition(
            &asked(&servers, &|| true),
            &root,
            "src/engine.rs",
            line,
            column,
        )
        .unwrap();
        assert_eq!(found.path, "src/engine.rs");
        assert!(found.lines.lines[found.line].contains("fn balance("));
    }

    /// A stored grant gone once the language server answered, as when another window revoked
    /// it while the lookup waited, drops the answer and stops the servers.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_revocation_while_the_server_answers_drops_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        let servers = Servers::default();
        // A server that ends at once, so it answers the handshake with a failure.
        let answering = LanguageServer::stand_in("true", &[], dir.path()).unwrap();
        drop(servers.insert("rust-analyzer", answering));
        let other = servers.insert(
            "gopls",
            LanguageServer::stand_in("cat", &[], dir.path()).unwrap(),
        );
        let process = other.lock().process_id();
        drop(other);
        // The grant applies as the lookup is admitted, and is gone once the server answered.
        let reads = std::cell::Cell::new(0);
        let trusted = || {
            reads.set(reads.get() + 1);
            reads.get() == 1
        };
        let found = find_definition(&asked(&servers, &trusted), dir.path(), "lib.rs", 0, 3);
        assert!(matches!(found, Err(Unanswered::Denied)));
        assert_eq!(servers.len(), 0);
        assert!(
            !Path::new(&format!("/proc/{process}")).exists(),
            "a server still runs"
        );
    }

    /// No language server starts once the stored grant no longer applies, as when trust was
    /// revoked in another window: the lookup finds trust denied before starting one.
    #[test]
    fn no_server_starts_once_trust_is_revoked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        let servers = Servers::default();
        let found = find_definition(&asked(&servers, &|| false), dir.path(), "lib.rs", 0, 3);
        assert!(matches!(found, Err(Unanswered::Denied)));
        assert_eq!(servers.len(), 0);
    }
}
