//! The whole file as a read-only vim buffer: the head version with change markers in the
//! gutter, vim motions, find, and go-to-definition into other files with a jump back.

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use review_core::UnitId;

use super::effects::{Outcome, Subject};
use super::{DEFINITION_DEADLINE, Mode, Model, Running, ServerKey, Servers};
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
    /// selected file: the hunk of the diff's rows now that holds the line's unit.
    #[must_use]
    pub fn buffer_hunk(&self) -> Option<usize> {
        let buffer = self.buffer.as_ref()?;
        if buffer.origin != Origin::Review(self.selected.clone()?) {
            return None;
        }
        let unit = buffer
            .markers
            .get(buffer.cursor.row)
            .copied()
            .flatten()?
            .unit();
        self.hunk_of(unit)
    }

    /// The display hunk of the shown rows that holds `unit`.
    #[must_use]
    pub(crate) fn hunk_of(&self, unit: UnitId) -> Option<usize> {
        self.rows
            .hunks
            .iter()
            .position(|units| units.contains(&unit))
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
        let mut markers = vec![None; lines.lines.len().max(1)];
        let mut start_row = None;
        for (hunk, hunk_units) in self.rows.hunks.iter().enumerate() {
            for &unit in hunk_units {
                let Some(chunk) = view.unit(unit).ok().and_then(|record| record.chunk()) else {
                    continue;
                };
                if hunk == self.hunk && start_row.is_none() {
                    start_row = Some(chunk.new.start);
                }
                if chunk.new.len == 0 {
                    let at = chunk.new.start.min(markers.len().saturating_sub(1));
                    if let Some(slot) = markers.get_mut(at) {
                        slot.get_or_insert(Marker::Removed(unit));
                    }
                }
                for row in chunk.new.range() {
                    if let Some(slot) = markers.get_mut(row) {
                        *slot = Some(Marker::Added(unit));
                    }
                }
            }
        }
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
    fn leave_buffer(&mut self) {
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
        if !self.trusted && self.is_checkout() {
            self.ask_trust(true);
            return;
        }
        if !self.trusted {
            self.set_status(
                "Go to definition needs a language server, which runs code from this review. \
                 Press Ctrl+T to trust it first.",
            );
            return;
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
        let (Some(review), Some(record)) = (
            self.review.clone(),
            self.snapshot.as_ref().and_then(|snapshot| {
                snapshot
                    .history
                    .revisions()
                    .into_iter()
                    .find(|record| record.number == snapshot.revision)
                    .cloned()
            }),
        ) else {
            return;
        };
        // The answer lands only in the buffer and snapshot it was asked for.
        let lookup = Lookup {
            buffer: buffer.id,
            shown: self.shown,
        };
        let servers = std::sync::Arc::clone(&self.servers);
        self.looking_up = true;
        self.set_status("Looking up the definition; the language server may need to index first…");
        self.spawn(Subject::Lookup(lookup), move || {
            let found = review.repo().tree_of(&record.head).and_then(|tree| {
                let root = review.tool_worktree(&tree)?;
                Ok((tree, root))
            });
            Outcome::Definition(
                found
                    .map_err(|err| err.to_string())
                    .and_then(|(tree, root)| {
                        // Trust may be revoked while the worktree is written, or while the server
                        // starts, so it is asked again at each.
                        let trusted = || review.trust_for(&tree).is_ok_and(|trust| trust.trusted);
                        find_definition(&servers, &root, &path, row, col, &trusted)
                    }),
            )
        });
    }

    /// Takes in the answer of a definition lookup for the buffer and revision that show, while
    /// the review is trusted.
    pub(super) fn definition_found(&mut self, found: Result<Definition, String>) {
        match found {
            Ok(found) => self.show_definition(found),
            Err(err) => self.fail(format!("Go to definition: {err}")),
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

/// Why a definition lookup stopped: trust was revoked while it ran.
const REVOKED_TRUST: &str = "trust was revoked, so no language server runs";

/// Returns the running server `key` names, launching it with `launch` when none runs, while
/// `trusted` says tools may run, once `initialize` completed its handshake.
///
/// Trust is checked, and a server launched, under the servers' lock, which a revocation holds
/// while it records itself and stops the servers (see [`revoke_servers`]): a revocation either
/// comes first and is seen here, or waits for the launch, which only spawns the process, and
/// stops the server it launched.
/// The handshake runs after the lock is released, with the server in the map already, so a
/// revocation meanwhile stops it there and the handshake fails.
/// A server already running gets no request either once trust is revoked.
fn server_for(
    servers: &Servers,
    key: &ServerKey,
    trusted: &dyn Fn() -> bool,
    launch: impl FnOnce() -> Result<LanguageServer, String>,
    initialize: impl FnOnce(&mut LanguageServer) -> Result<(), String>,
) -> Result<Arc<Mutex<LanguageServer>>, String> {
    let server = {
        let mut running = servers.lock();
        if !trusted() {
            return Err(REVOKED_TRUST.to_owned());
        }
        if let Some(found) = running.get(key) {
            Arc::clone(&found.server)
        } else {
            let launched = Running::new(launch()?);
            let server = Arc::clone(&launched.server);
            running.insert(key.clone(), launched);
            server
        }
    };
    let initialized = initialize(&mut server.lock());
    if let Err(err) = initialized {
        forget(servers, key, &server);
        return Err(err);
    }
    Ok(server)
}

/// Removes `server` from `servers` under `key`, unless another took its place; it stops once
/// nothing holds it.
fn forget(servers: &Servers, key: &ServerKey, server: &Arc<Mutex<LanguageServer>>) {
    let mut running = servers.lock();
    if running
        .get(key)
        .is_some_and(|running| Arc::ptr_eq(&running.server, server))
    {
        running.remove(key);
    }
}

/// Records a revocation of trust with `record` and stops the language servers, under the
/// servers' lock, so a lookup that checks trust and launches a server under it either sees the
/// revocation or has its server stopped here; `record` returns whether the servers stop, as
/// when no other grant still applies.
///
/// Every server stops at once, one a lookup is using or still shaking hands with included:
/// what waits on it fails.
pub(super) fn revoke_servers<E>(
    servers: &Servers,
    record: impl FnOnce() -> Result<bool, E>,
) -> Result<(), E> {
    let mut running = servers.lock();
    if record()? {
        // Each server stops as it leaves the map.
        running.clear();
    }
    Ok(())
}

/// Finds where the identifier at byte `column` of line `line` of `path` (relative to `root`) is
/// defined, starting a language server for `root` on first use, and reads that file.
///
/// `trusted` says whether tools may still run on `root`: a server starts only while they may,
/// and one that answers once they may not is stopped instead of kept.
fn find_definition(
    servers: &Servers,
    root: &Path,
    path: &str,
    line: usize,
    column: usize,
    trusted: &dyn Fn() -> bool,
) -> Result<Definition, String> {
    // Language servers answer with canonical paths, so the root must be canonical to strip it.
    let root = &root
        .canonicalize()
        .map_err(|err| format!("cannot open {}: {err}", root.display()))?;
    let file = root.join(path);
    let command = lsp::server_for(&file)
        .ok_or_else(|| format!("no language server is configured for {path}"))?;
    let text =
        std::fs::read_to_string(&file).map_err(|err| format!("cannot read {path}: {err}"))?;
    let source = text
        .lines()
        .nth(line)
        .ok_or_else(|| format!("{path} has no line {}", line + 1))?;
    let position = lsp::Position {
        line,
        character: lsp::utf16_column(source, column.min(source.len())),
    };
    let key = (command.program, root.clone());
    let server = server_for(
        servers,
        &key,
        trusted,
        || LanguageServer::launch(&command, root).map_err(|err| err.to_string()),
        |server| {
            server
                .initialize(&command, root)
                .map_err(|err| err.to_string())
        },
    )?;
    let definition = server
        .lock()
        .definition(&file, &text, position, DEFINITION_DEADLINE)
        .map_err(|err| err.to_string());
    if !trusted() {
        // A revocation stopped the servers it found; this one stops once the lookup lets go
        // of it, which dropping does.
        forget(servers, &key, &server);
        return Err(REVOKED_TRUST.to_owned());
    }
    let definition = definition?;
    drop(server);
    let target = std::fs::read_to_string(&definition.path)
        .map_err(|err| format!("cannot read {}: {err}", definition.path.display()))?;
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
    #[cfg(target_os = "linux")]
    use std::time::Duration;

    use super::*;

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
        let found = find_definition(
            &Servers::default(),
            &root,
            "src/engine.rs",
            line,
            column,
            &|| true,
        )
        .unwrap();
        assert_eq!(found.path, "src/engine.rs");
        assert!(found.lines.lines[found.line].contains("fn balance("));
    }

    /// A revocation while a lookup checks trust and launches a server, under the servers' lock,
    /// waits for the launch and stops the server it launched; the lookup refuses from then on,
    /// a running server included.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_revocation_waits_for_a_server_starting_and_stops_it() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let key: ServerKey = ("rust", dir.path().to_owned());
        let revoked = Arc::new(AtomicBool::new(false));
        let launched = Arc::new(AtomicBool::new(false));
        let (launching, launch_begun) = std::sync::mpsc::channel();
        let worker = std::thread::spawn({
            let servers = Arc::clone(&servers);
            let key = key.clone();
            let revoked = Arc::clone(&revoked);
            let launched = Arc::clone(&launched);
            let root = dir.path().to_owned();
            move || {
                let trusted = || !revoked.load(Ordering::SeqCst);
                let launch = || {
                    launching.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    let server = LanguageServer::stand_in("cat", &[], &root);
                    launched.store(true, Ordering::SeqCst);
                    server.map_err(|err| err.to_string())
                };
                server_for(&servers, &key, &trusted, launch, |_| Ok(())).map(drop)
            }
        });

        launch_begun.recv().unwrap();
        revoke_servers(&servers, || {
            // The launch under the lock finished before the revocation recorded itself.
            assert!(launched.load(Ordering::SeqCst));
            revoked.store(true, Ordering::SeqCst);
            Ok::<_, ()>(true)
        })
        .unwrap();
        assert!(worker.join().unwrap().is_ok());
        assert!(servers.lock().is_empty(), "the server launched is stopped");

        // A server running still, as one started for another window, gets no request.
        let running = LanguageServer::stand_in("cat", &[], dir.path()).unwrap();
        servers.lock().insert(key.clone(), Running::new(running));
        let trusted = || !revoked.load(Ordering::SeqCst);
        let refused = server_for(
            &servers,
            &key,
            &trusted,
            || panic!("no server starts"),
            |_| panic!("no handshake runs"),
        );
        assert!(refused.is_err_and(|err| err.contains("trust was revoked")));
    }

    /// A revocation while a server is in its handshake, which runs outside the servers' lock,
    /// returns at once and stops it, and the handshake fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_revocation_during_a_slow_handshake_returns_promptly() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let servers = Servers::default();
        let key: ServerKey = ("rust", dir.path().to_owned());
        let revoked = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn({
            let servers = Arc::clone(&servers);
            let key = key.clone();
            let revoked = Arc::clone(&revoked);
            let root = dir.path().to_owned();
            move || {
                let trusted = || !revoked.load(Ordering::SeqCst);
                // A server that never answers the handshake.
                let launch = || {
                    LanguageServer::stand_in("sleep", &["30"], &root).map_err(|err| err.to_string())
                };
                let command = lsp::server_for(Path::new("lib.rs")).unwrap();
                let initialize = |server: &mut LanguageServer| {
                    server
                        .initialize(&command, &root)
                        .map_err(|err| err.to_string())
                };
                server_for(&servers, &key, &trusted, launch, initialize).map(drop)
            }
        });
        // The server is in the map, shaking hands.
        let waited = std::time::Instant::now();
        while servers.lock().is_empty() {
            assert!(
                waited.elapsed() < Duration::from_secs(10),
                "no server launched"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(100));

        let revoking = std::time::Instant::now();
        revoke_servers(&servers, || {
            revoked.store(true, Ordering::SeqCst);
            Ok::<_, ()>(true)
        })
        .unwrap();
        assert!(
            revoking.elapsed() < Duration::from_secs(1),
            "the revocation waited"
        );
        let shaken = worker.join().unwrap();
        assert!(
            shaken.is_err_and(|err| err.contains("stopped")),
            "the handshake fails"
        );
        assert!(waited.elapsed() < Duration::from_secs(10));
        assert!(servers.lock().is_empty());
    }

    /// No language server starts once trust is revoked, as while the worktree was written: the
    /// lookup fails before starting one.
    #[test]
    fn no_server_starts_once_trust_is_revoked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn a() {}\n").unwrap();
        let servers = Servers::default();
        let found = find_definition(&servers, dir.path(), "lib.rs", 0, 3, &|| false);
        assert!(found.is_err_and(|err| err.contains("trust was revoked")));
        assert!(servers.lock().is_empty());
    }
}
