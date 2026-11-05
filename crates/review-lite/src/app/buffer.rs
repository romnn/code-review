//! The whole file as a read-only vim buffer: the head version with change markers in the
//! gutter, vim motions, find, and go-to-definition into other files with a jump back.

use std::path::Path;
use std::sync::Arc;

use gpui::{Context, Window};
use gpui_component::scroll::ScrollbarHandle as _;

use super::text::{self, TextPos};
use super::{
    BufferBottom, BufferTop, CursorDown, CursorUp, DEFINITION_DEADLINE, Dismiss, GoToDefinition,
    JumpBack, Mode, NextWord, PreviousWord, ReviewApp, Servers, ToggleBuffer, scroll,
};
use crate::lsp::{self, LanguageServer};
use crate::rows::FullText;
use crate::syntax::SideSyntax;

/// What a line of a review buffer changed, for its gutter marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// The line was added or changed, in this display hunk.
    Added(usize),
    /// Lines were removed just before this line, in this display hunk.
    Removed(usize),
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
    pub id: u64,
    pub origin: Origin,
    pub lines: Arc<FullText>,
    pub syntax: Option<Arc<SideSyntax>>,
    /// Each line's change marker, for a review buffer.
    pub markers: Arc<Vec<Option<Marker>>>,
    pub cursor: TextPos,
    /// The buffer a definition jump left, for `Ctrl+O`.
    pub previous: Option<Box<Buffer>>,
}

impl Buffer {
    /// The title the buffer's header shows.
    pub fn title(&self) -> &str {
        match &self.origin {
            Origin::Review(path) | Origin::Definition(path) | Origin::Head(path) => path,
        }
    }

    /// The breadcrumb of the scopes around the cursor, such as `impl Ledger › fn balance`,
    /// counting a scope that starts on the cursor's line.
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

impl ReviewApp {
    /// The display hunk of the line under the buffer cursor, only in a review buffer of the
    /// selected file, whose markers index that file's rows.
    pub(super) fn buffer_hunk(&self) -> Option<usize> {
        let buffer = self.buffer.as_ref()?;
        if buffer.origin != Origin::Review(self.selected.clone()?) {
            return None;
        }
        match buffer.markers.get(buffer.cursor.row).copied().flatten()? {
            Marker::Added(hunk) | Marker::Removed(hunk) => Some(hunk),
        }
    }

    /// `=`: the whole file at the selected hunk, or back to the diff at the hunk under the
    /// cursor.
    pub(super) fn toggle_buffer(
        &mut self,
        _: &ToggleBuffer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.mode == Mode::Buffer {
            self.leave_buffer(window, cx);
        } else {
            self.enter_buffer(window, cx);
        }
    }

    /// Opens the selected file as a buffer, with the cursor on the selected hunk.
    pub(super) fn enter_buffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The buffer's markers come from the selected file's hunks, never a preview's header.
        self.finish_preview();
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
            cx.notify();
            return;
        };
        let Some(lines) = self.blobs.texts.get(&head.oid).cloned() else {
            self.set_status("This file is binary or not read yet.");
            cx.notify();
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
                        slot.get_or_insert(Marker::Removed(hunk));
                    }
                }
                for row in chunk.new.range() {
                    if let Some(slot) = markers.get_mut(row) {
                        *slot = Some(Marker::Added(hunk));
                    }
                }
            }
        }
        let row = start_row
            .unwrap_or(0)
            .min(lines.lines.len().saturating_sub(1));
        let buffer = Buffer {
            id: self.next_buffer_id(),
            origin: super::buffer::Origin::Review(path),
            syntax: self.blobs.syntax.get(&head.oid).cloned(),
            lines,
            markers: Arc::new(markers),
            cursor: TextPos { row, col: 0 },
            previous: None,
        };
        self.set_buffer(Some(buffer));
        self.buffer_animation = None;
        // The list may not be laid out yet, so the cursor is centered once it is.
        self.buffer_list
            .scroll_to_item_strict(row, gpui::ScrollStrategy::Center);
        self.enter_mode(Mode::Buffer, window, cx);
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
    fn leave_buffer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self.buffer.as_ref().map(|buffer| buffer.cursor.row);
        let hunk = self.buffer_hunk().or_else(|| {
            let row = row?;
            let buffer = self.buffer.as_ref()?;
            buffer
                .markers
                .iter()
                .take(row + 1)
                .rev()
                .find_map(|marker| match marker {
                    Some(Marker::Added(hunk) | Marker::Removed(hunk)) => Some(*hunk),
                    None => None,
                })
        });
        self.find = None;
        self.enter_mode(Mode::Diff, window, cx);
        if let Some(hunk) = hunk {
            self.select_hunk(hunk, cx);
        } else {
            self.reveal_hunk();
        }
    }

    /// Esc in the buffer: closes the find results, then returns from a definition, then goes
    /// back to the diff.
    pub(super) fn buffer_dismiss(
        &mut self,
        _: &Dismiss,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find.take().is_some() {
            cx.notify();
            return;
        }
        if self
            .selection
            .take()
            .is_some_and(|selection| !selection.is_empty())
        {
            cx.notify();
            return;
        }
        if self
            .buffer
            .as_ref()
            .is_some_and(|buffer| buffer.previous.is_some())
        {
            self.jump_back(&JumpBack, window, cx);
            return;
        }
        self.leave_buffer(window, cx);
    }

    /// Moves the cursor to `row`, keeping the column within the line, and scrolls to it.
    pub(super) fn set_buffer_cursor(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    /// Moves the cursor by `rows` lines.
    pub(super) fn move_buffer_cursor_by(&mut self, rows: isize, cx: &mut Context<Self>) {
        let Some(cursor) = self.buffer.as_ref().map(|buffer| buffer.cursor) else {
            return;
        };
        self.set_buffer_cursor(cursor.row.saturating_add_signed(rows), cursor.col, cx);
    }

    /// Scrolls the buffer so its cursor has some lines of air.
    fn reveal_buffer_cursor(&mut self) {
        let Some(row) = self.buffer.as_ref().map(|buffer| buffer.cursor.row) else {
            return;
        };
        let height = self.fonts.row_height();
        let viewport = f32::from(self.buffer_list.viewport_bounds().size.height);
        let current = self.buffer_animation.map_or_else(
            || scroll::offset_of(&self.buffer_list),
            |animation| animation.target(),
        );
        let top = scroll::float(row) * height;
        if let Some(target) = scroll::reveal(current, viewport, top, top + height, height * 4.0) {
            self.scroll_buffer_to(target);
        }
    }

    pub(super) fn cursor_down(&mut self, _: &CursorDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_buffer_cursor_by(1, cx);
    }

    pub(super) fn cursor_up(&mut self, _: &CursorUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_buffer_cursor_by(-1, cx);
    }

    pub(super) fn buffer_top(&mut self, _: &BufferTop, _: &mut Window, cx: &mut Context<Self>) {
        self.set_buffer_cursor(0, 0, cx);
    }

    pub(super) fn buffer_bottom(
        &mut self,
        _: &BufferBottom,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_buffer_cursor(usize::MAX, 0, cx);
    }

    /// `w` and `b`: the next or previous identifier, across lines.
    fn move_word(&mut self, forward: bool, cx: &mut Context<Self>) {
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
            self.set_buffer_cursor(row, col, cx);
        }
    }

    pub(super) fn next_word(&mut self, _: &NextWord, _: &mut Window, cx: &mut Context<Self>) {
        self.move_word(true, cx);
    }

    pub(super) fn previous_word(
        &mut self,
        _: &PreviousWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_word(false, cx);
    }

    /// Updates the review buffer's highlights once they are computed.
    pub(super) fn refresh_buffer_syntax(&mut self) {
        let Some(view) = self.view() else {
            return;
        };
        let Some(Origin::Review(path)) = self.buffer.as_ref().map(|buffer| buffer.origin.clone())
        else {
            return;
        };
        let units = view.units_of_file(&path);
        let oid = units
            .first()
            .and_then(|&unit| view.file(unit).ok())
            .and_then(|file| file.new.as_ref().map(|side| side.oid.clone()));
        let syntax = oid.and_then(|oid| self.blobs.syntax.get(&oid).cloned());
        if let Some(buffer) = &mut self.buffer {
            buffer.syntax = syntax;
        }
    }

    /// `g d`: asks a language server, in the background, where the identifier under the cursor
    /// is defined, and moves there, opening another file as a buffer when it is elsewhere.
    pub(super) fn go_to_definition(
        &mut self,
        _: &GoToDefinition,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.trust_pending = false;
        if !self.trusted {
            self.set_status(
                "Go to definition needs a language server, which runs code from this review. \
                 Press Ctrl+T to trust it first.",
            );
            cx.notify();
            return;
        }
        if self.looking_up {
            self.set_status("A definition lookup is still running.");
            cx.notify();
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
            cx.notify();
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
        // The answer applies only to the buffer and revision it was asked for.
        let request = (buffer.id, record.number);
        let servers = std::sync::Arc::clone(&self.servers);
        self.looking_up = true;
        self.set_status("Looking up the definition; the language server may need to index first…");
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            let root = review.worktree(&record).map_err(|err| err.to_string())?;
            find_definition(&servers, &root, &path, row, col)
        });
        cx.spawn(async move |this, cx| {
            let found = work.await;
            this.update(cx, |this, cx| {
                this.looking_up = false;
                if !this.trusted {
                    // Trust was revoked during the lookup: stop the servers, ignore the answer.
                    this.servers.lock().clear();
                    return;
                }
                let current = this
                    .buffer
                    .as_ref()
                    .map(|buffer| buffer.id)
                    .zip(this.snapshot.as_ref().map(|snapshot| snapshot.revision));
                if current != Some(request) {
                    this.set_status("The definition lookup finished for a file no longer shown.");
                    cx.notify();
                    return;
                }
                match found {
                    Ok(found) => this.show_definition(found, cx),
                    Err(err) => this.fail(format!("Go to definition: {err}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Moves to a definition: in this buffer when it is in the same file, otherwise in a buffer
    /// of the other file that `Ctrl+O` leaves.
    fn show_definition(&mut self, found: Definition, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    /// `Ctrl+O`: back to the buffer a definition jump left.
    pub(super) fn jump_back(&mut self, _: &JumpBack, _: &mut Window, cx: &mut Context<Self>) {
        let previous = self
            .buffer
            .as_mut()
            .and_then(|buffer| buffer.previous.take());
        if let Some(previous) = previous {
            self.set_buffer(Some(*previous));
            self.reveal_buffer_cursor();
            cx.notify();
        }
    }
}

/// Where a definition is, with its file's text.
struct Definition {
    path: String,
    line: usize,
    column: usize,
    lines: FullText,
    syntax: SideSyntax,
}

/// Finds where the identifier at byte `column` of line `line` of `path` (relative to `root`) is
/// defined, starting a language server for `root` on first use, and reads that file.
fn find_definition(
    servers: &Servers,
    root: &Path,
    path: &str,
    line: usize,
    column: usize,
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
    let mut servers = servers.lock();
    let key = (command.program, root.clone());
    if !servers.contains_key(&key) {
        let server = LanguageServer::start(&command, root).map_err(|err| err.to_string())?;
        servers.insert(key.clone(), server);
    }
    let server = servers.get_mut(&key).ok_or("the language server is gone")?;
    let definition = server
        .definition(&file, &text, position, DEFINITION_DEADLINE)
        .map_err(|err| err.to_string())?;
    drop(servers);
    let target = std::fs::read_to_string(&definition.path)
        .map_err(|err| format!("cannot read {}: {err}", definition.path.display()))?;
    let shown = definition
        .path
        .strip_prefix(root)
        .unwrap_or(&definition.path)
        .display()
        .to_string();
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

    /// Follows a method call in the demo to its definition; skipped where rust-analyzer is not
    /// installed.
    #[test]
    fn definition_from_a_demo_head_line() {
        if std::process::Command::new("rust-analyzer")
            .arg("--version")
            .output()
            .is_err()
        {
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
        let found =
            find_definition(&Servers::default(), &root, "src/engine.rs", line, column).unwrap();
        assert_eq!(found.path, "src/engine.rs");
        assert!(found.lines.lines[found.line].contains("fn balance("));
    }
}
