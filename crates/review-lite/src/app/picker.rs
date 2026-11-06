//! The pickers, telescope's two most used: `Ctrl+P` finds a file by a fuzzy match of its path,
//! and `Ctrl+G` searches file contents as you type, with a preview of the lines around each
//! hit. Both start on the review's changed files; Tab widens them to the whole head tree.
//!
//! Searching changed files reads the head texts the window already caches for the diff, the
//! same text the diff and the buffer show, so a hit's line is exactly the line Enter opens and
//! typing starts no process. The whole tree is not in memory, so there `git grep` searches the
//! pinned head snapshot and `git ls-tree` lists it, once per picker. Every search runs on the
//! background executor; a newer query cancels the older one, and only the newest result lands.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, FontWeight, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    StyledText, Subscription, UniformListScrollHandle, Window, div, px, relative, uniform_list,
};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{h_flex, v_flex};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use review_core::tree::NodeKind;
use review_core::{FileTone, ObjectId, Review};

use super::render::{GRID, Palette, progress_glyph};
use super::state::Progress;
use super::{
    Mode, Overlay, PickerBrowse, PickerClose, PickerEdit, PickerNext, PickerOpen, PickerPrevious,
    PickerScope, ReviewApp, buffer,
};
use crate::rows::FullText;

/// How many entries a search keeps; the list says when there were more.
const LIMIT: usize = 2000;

/// How long the content search waits for typing to pause before it searches.
const DEBOUNCE: Duration = Duration::from_millis(80);

/// How many lines the preview shows above a hit.
const PREVIEW_ABOVE: usize = 8;

/// How many lines the preview shows at most.
const PREVIEW_LINES: usize = 80;

/// The longest line text an entry keeps, in bytes.
const ENTRY_TEXT: usize = 240;

/// What a picker finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Files by a fuzzy match of their path.
    Files,
    /// Lines that contain the query.
    Content,
}

/// Which files a picker covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The review's changed files.
    Changed,
    /// Every file of the head snapshot.
    Tree,
}

/// How a changed file shows in a picker: its change and its review progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub letter: char,
    pub tone: FileTone,
    pub progress: Progress,
    pub reviewed: usize,
    pub total: usize,
}

/// A changed file a picker offers, with its head text when it has one.
#[derive(Clone)]
pub struct Candidate {
    /// The display path, as the tree shows it.
    pub path: String,
    pub change: Change,
    pub text: Option<Arc<FullText>>,
}

/// One entry of a picker's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    /// For a content hit, its zero-based line.
    pub line: Option<usize>,
    /// For a content hit, the byte column of its first match in the full line.
    pub column: usize,
    /// For a content hit, the line's text without its indentation.
    pub text: String,
    /// The bytes to highlight: of the path for a file, of `text` for a content hit.
    pub highlights: Vec<Range<usize>>,
    /// For a changed file, how it changed and how far its review is.
    pub change: Option<Change>,
}

/// The head snapshot's files, listed once when a picker first needs the whole tree.
pub struct Listing {
    pub paths: Vec<String>,
    pub oids: HashMap<String, ObjectId>,
}

/// An open picker.
pub struct Picker {
    pub kind: Kind,
    pub scope: Scope,
    pub input: Entity<InputState>,
    pub query: String,
    /// Whether the query takes typing; otherwise the list takes the keys, as vim's normal mode.
    pub editing: bool,
    pub entries: Arc<Vec<Entry>>,
    pub selected: usize,
    pub list: UniformListScrollHandle,
    /// Whether the search stopped at [`LIMIT`].
    pub truncated: bool,
    pub searching: bool,
    pub error: Option<String>,
    /// The revision the picker searches; a result for another one is dropped.
    revision: review_core::RevisionNumber,
    head: ObjectId,
    /// This picker, among every picker the window opened, so an answer for a closed one is
    /// never applied to a newer one.
    id: u64,
    /// The search in flight, numbered from the same window-wide count as `id`.
    generation: u64,
    cancel: Arc<AtomicBool>,
    changed: Arc<Vec<Candidate>>,
    listing: Option<Arc<Listing>>,
    /// Texts of unchanged files, read for the preview.
    texts: HashMap<String, Arc<FullText>>,
    /// Files whose text could not be read (binary or failing), so they are not read again.
    unreadable: HashSet<String>,
    reading: Option<String>,
    /// Enter came while a search ran: the entry opens once the newest result lands.
    open_when_found: bool,
    _subscription: Subscription,
}

/// What one search needs, moved to the background.
struct Query {
    kind: Kind,
    scope: Scope,
    text: String,
    review: Arc<Review>,
    head: ObjectId,
    changed: Arc<Vec<Candidate>>,
    listing: Option<Arc<Listing>>,
    cancel: Arc<AtomicBool>,
}

/// What one search found.
struct Found {
    entries: Vec<Entry>,
    truncated: bool,
    listing: Option<Arc<Listing>>,
    error: Option<String>,
}

impl Picker {
    /// Whether a search or a preview read is running.
    pub fn busy(&self) -> bool {
        self.searching || self.reading.is_some()
    }

    /// The selected entry.
    pub fn entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// The head text of `path`, when it is in memory.
    fn text(&self, path: &str) -> Option<Arc<FullText>> {
        self.changed
            .iter()
            .find(|candidate| candidate.path == path)
            .and_then(|candidate| candidate.text.clone())
            .or_else(|| self.texts.get(path).cloned())
    }
}

impl ReviewApp {
    /// `Ctrl+P`: the file finder.
    pub(super) fn find_files(
        &mut self,
        _: &super::FindFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker(Kind::Files, window, cx);
    }

    /// `Ctrl+G`: the content search.
    pub(super) fn search_content(
        &mut self,
        _: &super::SearchContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_picker(Kind::Content, window, cx);
    }

    fn open_picker(&mut self, kind: Kind, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(snapshot), Some(record)) = (self.snapshot.as_ref(), self.revision_record())
        else {
            self.set_status("The review is not open yet.");
            cx.notify();
            return;
        };
        let revision = snapshot.revision;
        // One overlay owns the keys at a time.
        if self.settings_panel.is_some() {
            self.close_settings(window, cx);
        }
        let changed = Arc::new(self.candidates());
        let placeholder = match kind {
            Kind::Files => "Find a file",
            Kind::Content => "Search file contents",
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&input, window, Self::picker_event);
        self.picker_runs += 1;
        self.picker = Some(Picker {
            kind,
            scope: Scope::Changed,
            input,
            query: String::new(),
            editing: true,
            entries: Arc::new(Vec::new()),
            selected: 0,
            list: UniformListScrollHandle::new(),
            truncated: false,
            searching: false,
            error: None,
            revision,
            head: record.head,
            id: self.picker_runs,
            generation: self.picker_runs,
            cancel: Arc::new(AtomicBool::new(false)),
            changed,
            listing: None,
            texts: HashMap::new(),
            unreadable: HashSet::new(),
            reading: None,
            open_when_found: false,
            _subscription: subscription,
        });
        self.overlay = Some(Overlay::Picker);
        self.selection = None;
        self.focus_mode(window, cx);
        self.run_picker(Duration::ZERO, window, cx);
        cx.notify();
    }

    /// The shown revision's record.
    fn revision_record(&self) -> Option<review_core::RevisionRecord> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot
            .history
            .revisions()
            .into_iter()
            .find(|record| record.number == snapshot.revision)
            .cloned()
    }

    /// The changed files in tree order, with their change, progress and head text.
    fn candidates(&self) -> Vec<Candidate> {
        let Some(view) = self.view() else {
            return Vec::new();
        };
        // Each changed file's head blob by display path, in one pass over the analysis.
        let heads: HashMap<&str, &ObjectId> = view
            .analysis()
            .map(|analysis| {
                analysis
                    .files
                    .iter()
                    .filter_map(|file| Some((file.display_path(), &file.new.as_ref()?.oid)))
                    .collect()
            })
            .unwrap_or_default();
        self.tree
            .iter()
            .filter(|node| node.kind == NodeKind::File)
            .map(|node| {
                let head = heads.get(node.path.as_str());
                let progress = Progress::of(&node.counts);
                Candidate {
                    path: node.path.clone(),
                    change: Change {
                        letter: node
                            .change
                            .as_ref()
                            .map_or(' ', |change| change.kind.letter()),
                        tone: node
                            .change
                            .as_ref()
                            .map_or(node.tone, review_core::FileChangeSummary::tone),
                        progress,
                        reviewed: node.counts.reviewed + node.counts.accepted + node.counts.flagged,
                        total: node.counts.units,
                    },
                    text: head.and_then(|oid| self.blobs.texts.get(*oid).cloned()),
                }
            })
            .collect()
    }

    fn picker_event(
        &mut self,
        input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                let Some(picker) = &mut self.picker else {
                    return;
                };
                picker.query = query;
                let delay = match picker.kind {
                    Kind::Files => Duration::ZERO,
                    Kind::Content => DEBOUNCE,
                };
                self.run_picker(delay, window, cx);
            }
            InputEvent::PressEnter { .. } => self.picker_open(&PickerOpen, window, cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    /// Searches for the picker's query after `delay`, cancelling the search before it.
    fn run_picker(&mut self, delay: Duration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.review.clone() else {
            return;
        };
        self.picker_runs += 1;
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.cancel.store(true, Ordering::Relaxed);
        picker.cancel = Arc::new(AtomicBool::new(false));
        picker.generation = self.picker_runs;
        picker.searching = true;
        let generation = picker.generation;
        let query = Query {
            kind: picker.kind,
            scope: picker.scope,
            text: picker.query.clone(),
            review,
            head: picker.head.clone(),
            changed: Arc::clone(&picker.changed),
            listing: picker.listing.clone(),
            cancel: Arc::clone(&picker.cancel),
        };
        let current = move |this: &Self| {
            this.picker
                .as_ref()
                .is_some_and(|picker| picker.generation == generation)
        };
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
                let still = this.update(cx, |this, _| current(this)).unwrap_or(false);
                if !still {
                    return;
                }
            }
            let found = cx
                .background_executor()
                .spawn(async move { search(&query) })
                .await;
            cx.update_window(window_handle, |_, window, cx| {
                this.update(cx, |this, cx| {
                    if !current(this) {
                        return;
                    }
                    let mut open = false;
                    if let Some(picker) = &mut this.picker {
                        picker.entries = Arc::new(found.entries);
                        picker.truncated = found.truncated;
                        picker.error = found.error;
                        picker.searching = false;
                        if found.listing.is_some() {
                            picker.listing = found.listing;
                        }
                        picker.selected = 0;
                        picker.list.scroll_to_item(0, gpui::ScrollStrategy::Top);
                        open = std::mem::take(&mut picker.open_when_found);
                    }
                    if open {
                        this.picker_open(&PickerOpen, window, cx);
                    } else {
                        this.read_preview(cx);
                    }
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    /// Reads, in the background, the text of the selected entry's file when the preview needs
    /// it and it is not in memory.
    fn read_preview(&mut self, cx: &mut Context<Self>) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let Some(picker) = &mut self.picker else {
            return;
        };
        if picker.kind != Kind::Content || picker.reading.is_some() {
            return;
        }
        let Some(path) = picker.entry().map(|entry| entry.path.clone()) else {
            return;
        };
        if picker.text(&path).is_some() || picker.unreadable.contains(&path) {
            return;
        }
        let Some(oid) = picker
            .listing
            .as_ref()
            .and_then(|listing| listing.oids.get(&path).cloned())
        else {
            return;
        };
        picker.reading = Some(path.clone());
        let id = picker.id;
        let work = cx
            .background_executor()
            .spawn(async move { read_text(&review, &oid) });
        cx.spawn(async move |this, cx| {
            let text = work.await;
            this.update(cx, |this, cx| {
                let Some(picker) = this.picker.as_mut().filter(|picker| picker.id == id) else {
                    return;
                };
                picker.reading = None;
                match text {
                    Some(text) => {
                        picker.texts.insert(path, Arc::new(text));
                    }
                    None => {
                        picker.unreadable.insert(path);
                    }
                }
                // The selection may have moved on while this file was read.
                this.read_preview(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn picker_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let count = picker.entries.len();
        if count == 0 {
            return;
        }
        picker.selected = if forward {
            (picker.selected + 1) % count
        } else {
            (picker.selected + count - 1) % count
        };
        picker
            .list
            .scroll_to_item(picker.selected, gpui::ScrollStrategy::Nearest);
        self.read_preview(cx);
        cx.notify();
    }

    pub(super) fn picker_next(&mut self, _: &PickerNext, _: &mut Window, cx: &mut Context<Self>) {
        self.picker_step(true, cx);
    }

    pub(super) fn picker_previous(
        &mut self,
        _: &PickerPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker_step(false, cx);
    }

    /// Tab: between the changed files and the whole tree.
    pub(super) fn picker_scope(
        &mut self,
        _: &PickerScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        picker.scope = match picker.scope {
            Scope::Changed => Scope::Tree,
            Scope::Tree => Scope::Changed,
        };
        self.run_picker(Duration::ZERO, window, cx);
        cx.notify();
    }

    /// Esc while typing: the list takes the keys.
    pub(super) fn picker_browse(
        &mut self,
        _: &PickerBrowse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = &mut self.picker {
            picker.editing = false;
        }
        self.focus_mode(window, cx);
        cx.notify();
    }

    /// `i` or `/` in the list: back to typing.
    pub(super) fn picker_edit(
        &mut self,
        _: &PickerEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(picker) = &mut self.picker {
            picker.editing = true;
        }
        self.focus_mode(window, cx);
        cx.notify();
    }

    pub(super) fn picker_close(
        &mut self,
        _: &PickerClose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_picker(window, cx);
    }

    pub(super) fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = self.picker.take() {
            picker.cancel.store(true, Ordering::Relaxed);
        }
        self.overlay = None;
        self.focus_mode(window, cx);
        cx.notify();
    }

    /// Enter: opens the selected entry. A changed file opens in the diff, at the hunk that holds
    /// a content hit, or in the buffer at the hit's line when no hunk shows it; a file outside
    /// the review opens read-only in the buffer.
    pub(super) fn picker_open(
        &mut self,
        _: &PickerOpen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(picker) = &self.picker else {
            return;
        };
        // The entries belong to the query before the one being searched; the newest result
        // opens once it lands.
        if picker.searching {
            if let Some(picker) = &mut self.picker {
                picker.open_when_found = true;
            }
            return;
        }
        let Some(entry) = picker.entry().cloned() else {
            return;
        };
        let current = self.snapshot.as_ref().map(|snapshot| snapshot.revision);
        let oid = picker
            .listing
            .as_ref()
            .and_then(|listing| listing.oids.get(&entry.path).cloned());
        if current != Some(picker.revision) {
            self.close_picker(window, cx);
            self.set_status("The review moved to another revision; search again.");
            return;
        }
        // Everything an entry needs is checked before the picker closes.
        let column = entry.column;
        if entry.change.is_some() {
            self.close_picker(window, cx);
            self.open_changed(&entry.path, entry.line, column, window, cx);
        } else if let Some(oid) = oid {
            self.close_picker(window, cx);
            self.open_unchanged(entry.path, oid, entry.line.unwrap_or(0), column, window, cx);
        } else {
            self.fail(format!(
                "{} is not in the head snapshot's listing.",
                entry.path
            ));
            cx.notify();
        }
    }

    /// Shows a changed file's diff, at the hunk that changed head line `line` when one did,
    /// else opens it as a buffer at that line.
    fn open_changed(
        &mut self,
        path: &str,
        line: Option<usize>,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_file(path);
        self.set_status(match line {
            Some(line) => format!("{path}:{}", line + 1),
            None => path.to_owned(),
        });
        let Some(line) = line else {
            self.enter_mode(Mode::Diff, window, cx);
            self.reveal_hunk();
            return;
        };
        // The hunk whose units changed that head line, even when it is folded.
        let hunk = self.view().and_then(|view| {
            self.rows.hunks.iter().position(|units| {
                units.iter().any(|&unit| {
                    view.unit(unit)
                        .ok()
                        .and_then(|record| record.chunk())
                        .is_some_and(|chunk| chunk.new.range().contains(&line))
                })
            })
        });
        let row = self.rows.rows.iter().position(|row| {
            row.cells()
                .any(|cell| cell.head.as_ref().is_some_and(|head| head.line == line))
        });
        if let Some(hunk) = hunk {
            self.enter_mode(Mode::Diff, window, cx);
            self.select_hunk(hunk, cx);
            if let Some(row) = row.filter(|row| {
                self.hunk_rows
                    .get(hunk)
                    .is_some_and(|rows| rows.contains(row))
            }) {
                self.reveal_match(row);
            }
        } else {
            self.enter_buffer(window, cx);
            let opened = self.mode == Mode::Buffer
                && self
                    .buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.origin == buffer::Origin::Review(path.to_owned()));
            if opened {
                self.set_buffer_cursor(line, column, cx);
            } else {
                // The file cannot show in full (binary, or not read): its diff shows instead,
                // never a buffer of another file.
                self.enter_mode(Mode::Diff, window, cx);
                self.reveal_hunk();
                self.set_status(format!(
                    "{path}:{} is outside the diff, and the file cannot show in full.",
                    line + 1
                ));
            }
        }
        cx.notify();
    }

    /// Opens a file the review did not change as a read-only buffer at `line`, reading and
    /// highlighting it in the background.
    fn open_unchanged(
        &mut self,
        path: String,
        oid: ObjectId,
        line: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.review.clone() else {
            return;
        };
        let revision = self.snapshot.as_ref().map(|snapshot| snapshot.revision);
        self.set_status(format!("Opening {path}…"));
        let read_path = path.clone();
        self.highlighting += 1;
        let work = cx.background_executor().spawn(async move {
            let text = read_text(&review, &oid)?;
            let syntax = review_core::syntax::Language::from_path(&read_path)
                .map(|_| Arc::new(crate::syntax::analyze(&read_path, &text.lines.join("\n"))));
            Some((text, syntax))
        });
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let read = work.await;
            cx.update_window(window_handle, |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.highlighting = this.highlighting.saturating_sub(1);
                    if this.snapshot.as_ref().map(|snapshot| snapshot.revision) != revision {
                        return;
                    }
                    let Some((text, syntax)) = read else {
                        this.fail(format!("{path} is binary or could not be read."));
                        cx.notify();
                        return;
                    };
                    let buffer = buffer::Buffer {
                        id: this.next_buffer_id(),
                        origin: buffer::Origin::Head(path.clone()),
                        lines: Arc::new(text),
                        syntax,
                        markers: Arc::new(Vec::new()),
                        cursor: super::text::TextPos { row: 0, col: 0 },
                        previous: None,
                    };
                    this.set_buffer(Some(buffer));
                    this.buffer_animation = None;
                    this.enter_mode(Mode::Buffer, window, cx);
                    this.set_buffer_cursor(line, column, cx);
                    this.set_status(format!("{path}:{} · not changed in this review", line + 1));
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    /// The picker overlay: the query, the entries and, for the content search, a preview.
    pub(super) fn render_picker(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let picker = self.picker.as_ref()?;
        let colors = self.palette();
        let small = self.fonts.ui_size();
        let row_height = self.fonts.row_height();
        let entries = Arc::clone(&picker.entries);
        let kind = picker.kind;
        let list = uniform_list(
            "picker",
            entries.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                this.render_picker_rows(range, cx)
            }),
        )
        .track_scroll(&picker.list)
        .size_full();
        let preview = (kind == Kind::Content).then(|| {
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .border_l_1()
                .border_color(colors.hairline)
                .bg(colors.background)
                .child(self.render_picker_preview(row_height))
        });
        let query = self.render_picker_query(picker);
        let mode = if picker.editing {
            "typing · Esc for the list"
        } else {
            "list · i to type · Esc closes"
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(GRID * 16.0))
                .bg(backdrop(&colors))
                .child(
                    v_flex()
                        .w(px(GRID * 300.0))
                        .max_w(relative(0.94))
                        .h(relative(0.75))
                        .bg(colors.surface)
                        .border_1()
                        .border_color(frame_border(&colors, picker.editing))
                        .text_size(px(small))
                        .child(query)
                        .child(
                            h_flex()
                                .flex_1()
                                .min_h_0()
                                .child(
                                    div()
                                        .relative()
                                        .when(kind == Kind::Content, |list| list.w(relative(0.5)))
                                        .when(kind == Kind::Files, gpui::Styled::flex_1)
                                        .h_full()
                                        .min_w_0()
                                        .child(list)
                                        .child(
                                            Scrollbar::vertical(&picker.list)
                                                .mode(ScrollbarMode::Always),
                                        ),
                                )
                                .children(preview),
                        )
                        .child(
                            div()
                                .h(px(self.fonts.strip()))
                                .flex_none()
                                .flex()
                                .items_center()
                                .px(px(GRID * 3.0))
                                .border_t_1()
                                .border_color(colors.hairline)
                                .text_color(colors.faint)
                                .truncate()
                                .child(mode),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The picker's query line: what it finds, the query, the count and the scope.
    fn render_picker_query(&self, picker: &Picker) -> gpui::Div {
        let colors = self.palette();
        let entries = &picker.entries;
        let kind = picker.kind;
        let title = match picker.kind {
            Kind::Files => "files",
            Kind::Content => "grep",
        };
        let scope = match picker.scope {
            Scope::Changed => "changed files",
            Scope::Tree => "whole tree",
        };
        let count = if picker.searching {
            "searching…".to_owned()
        } else if let Some(error) = &picker.error {
            error.clone()
        } else if picker.truncated {
            format!("first {}", entries.len())
        } else {
            super::plural(
                entries.len(),
                if kind == Kind::Files { "file" } else { "hit" },
            )
        };
        h_flex()
            .h(px(self.fonts.bar() + GRID * 2.0))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .whitespace_nowrap()
            .border_b_1()
            .border_color(colors.hairline)
            .child(
                div()
                    .flex_none()
                    .text_color(colors.accent)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{title} ›")),
            )
            .child(div().flex_1().min_w_0().child(super::render::text_field(
                &picker.input,
                self.fonts.ui_size(),
            )))
            .child(div().flex_none().text_color(colors.dim).child(count))
            .child(
                div()
                    .flex_none()
                    .px(px(GRID * 2.0))
                    .bg(colors.raised)
                    .text_color(colors.text)
                    .child(scope),
            )
    }

    /// The picker's entries in `range`.
    fn render_picker_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(picker) = self.picker.as_ref() else {
            return Vec::new();
        };
        let colors = self.palette();
        let row_height = self.fonts.row_height();
        let entries = Arc::clone(&picker.entries);
        let selected = picker.selected;
        let letter_width = (self.fonts.ui_cell() * 1.5).round();
        let glyph_width = (self.fonts.ui_cell() * 1.75).round();
        range
            .filter_map(|index| {
                let entry = entries.get(index)?;
                let change = entry.change;
                let letter = change.map_or(SharedString::from(" "), |change| {
                    SharedString::from(change.letter.to_string())
                });
                let tone = change.map_or(colors.faint, |change| colors.tone(change.tone));
                let row = h_flex()
                    .id(("picker-entry", index))
                    .h(px(row_height))
                    .w_full()
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 2.0))
                    .cursor_pointer()
                    .when(index == selected, |row| row.bg(colors.raised))
                    .hover(|row| row.bg(colors.hairline))
                    .child(
                        div()
                            .w(px(letter_width))
                            .flex_none()
                            .text_color(tone)
                            .child(letter),
                    )
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            if let Some(picker) = &mut this.picker {
                                picker.selected = index;
                            }
                            if event.click_count() > 1 {
                                this.picker_open(&PickerOpen, window, cx);
                            } else {
                                this.read_preview(cx);
                            }
                            cx.notify();
                        }),
                    );
                let row =
                    match entry.line {
                        None => row
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(change.map_or(colors.dim, |change| {
                                        colors.progress(change.progress)
                                    }))
                                    .child(highlighted(&entry.path, &entry.highlights, &colors)),
                            )
                            .children(change.map(|change| {
                                h_flex()
                                    .flex_none()
                                    .gap(px(GRID * 2.0))
                                    .text_color(colors.progress(change.progress))
                                    .child(format!("{}/{}", change.reviewed, change.total))
                                    // A column as wide as the tree's, so the glyph stays
                                    // clear of the scrollbar over the row's end.
                                    .child(
                                        div()
                                            .w(px(glyph_width))
                                            .text_center()
                                            .child(progress_glyph(change.progress)),
                                    )
                            })),
                        Some(line) => row
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(relative(0.45))
                                    .truncate()
                                    .text_color(colors.dim)
                                    .child(format!("{}:{}", entry.path, line + 1)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(colors.text)
                                    .child(highlighted(&entry.text, &entry.highlights, &colors)),
                            ),
                    };
                Some(row.into_any_element())
            })
            .collect()
    }

    /// The lines around the selected content hit.
    fn render_picker_preview(&self, row_height: f32) -> gpui::AnyElement {
        let colors = self.palette();
        let Some(picker) = self.picker.as_ref() else {
            return div().into_any_element();
        };
        let Some(entry) = picker.entry() else {
            return div().into_any_element();
        };
        let line = entry.line.unwrap_or(0);
        let Some(text) = picker.text(&entry.path) else {
            let note = if picker.unreadable.contains(&entry.path) {
                "Binary, or could not be read."
            } else {
                "Reading the file…"
            };
            return div()
                .p(px(GRID * 3.0))
                .text_color(colors.faint)
                .child(note)
                .into_any_element();
        };
        let start = line.saturating_sub(PREVIEW_ABOVE);
        let needle = picker.query.clone();
        let ignore_case = !needle.chars().any(char::is_uppercase);
        let digits = (start + PREVIEW_LINES)
            .min(text.lines.len())
            .to_string()
            .len();
        v_flex()
            .size_full()
            .overflow_hidden()
            .font(self.fonts.code.clone())
            .text_size(px(self.fonts.code_size))
            .child(
                div()
                    .h(px(row_height))
                    .flex_none()
                    .px(px(GRID * 3.0))
                    .text_color(colors.dim)
                    .text_size(px(self.fonts.ui_size()))
                    .truncate()
                    .child(entry.path.clone()),
            )
            .children(
                text.lines
                    .iter()
                    .enumerate()
                    .skip(start)
                    .take(PREVIEW_LINES)
                    .map(|(index, content)| {
                        let hit = index == line;
                        let shown: String = content.chars().take(400).collect();
                        let ranges = if hit {
                            occurrences(&shown, &needle, ignore_case)
                        } else {
                            Vec::new()
                        };
                        h_flex()
                            .h(px(row_height))
                            .flex_none()
                            .when(hit, |row| row.bg(colors.raised))
                            .child(
                                div()
                                    .flex_none()
                                    .whitespace_nowrap()
                                    .px(px(GRID * 2.0))
                                    .text_color(if hit { colors.accent } else { colors.faint })
                                    .child(format!("{:>digits$}", index + 1)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_color(if hit { colors.text } else { colors.dim })
                                    .child(highlighted(&shown, &ranges, &colors)),
                            )
                    }),
            )
            .into_any_element()
    }
}

/// The backdrop over the window while a picker is open: the editor background, nearly opaque,
/// so the code behind never reads as part of the picker's preview.
fn backdrop(colors: &Palette) -> Hsla {
    Hsla {
        a: 0.85,
        ..colors.background
    }
}

/// The picker's border: the accent while its list has the keys, otherwise the dim text color,
/// either of which stands out from the backdrop and the picker's own surface in every theme.
fn frame_border(colors: &Palette, editing: bool) -> Hsla {
    if editing { colors.dim } else { colors.accent }
}

/// `text` with `ranges` drawn as matches.
fn highlighted(text: &str, ranges: &[Range<usize>], colors: &Palette) -> StyledText {
    let style = HighlightStyle {
        color: Some(colors.accent),
        font_weight: Some(FontWeight::BOLD),
        ..HighlightStyle::default()
    };
    let styles: Vec<_> = ranges
        .iter()
        .filter(|range| {
            range.end <= text.len()
                && text.is_char_boundary(range.start)
                && text.is_char_boundary(range.end)
        })
        .map(|range| (range.clone(), style))
        .collect();
    StyledText::new(text.to_owned()).with_highlights(styles)
}

/// Reads blob `oid` as text; `None` for a binary blob or a failed read.
fn read_text(review: &Review, oid: &ObjectId) -> Option<FullText> {
    let mut reader = review.repo().blob_reader().ok()?;
    match reader.read(oid).ok()? {
        review_core::git::Blob::Content(bytes) if !bytes.contains(&0) => {
            Some(FullText::new(&String::from_utf8_lossy(&bytes)))
        }
        _ => None,
    }
}

/// Runs one search.
fn search(query: &Query) -> Found {
    let mut found = Found {
        entries: Vec::new(),
        truncated: false,
        listing: None,
        error: None,
    };
    let mut listing = query.listing.clone();
    if query.scope == Scope::Tree && listing.is_none() {
        match query.review.repo().tree_files(&query.head) {
            Ok(files) => {
                let loaded = Arc::new(Listing {
                    oids: files
                        .iter()
                        .map(|file| (file.path.clone(), file.oid.clone()))
                        .collect(),
                    paths: files.into_iter().map(|file| file.path).collect(),
                });
                found.listing = Some(Arc::clone(&loaded));
                listing = Some(loaded);
            }
            Err(err) => found.error = Some(format!("listing the tree failed: {err}")),
        }
    }
    let changes: HashMap<&str, Change> = query
        .changed
        .iter()
        .map(|candidate| (candidate.path.as_str(), candidate.change))
        .collect();
    match query.kind {
        Kind::Files => {
            let mut candidates: Vec<(&str, Option<Change>)> = query
                .changed
                .iter()
                .map(|candidate| (candidate.path.as_str(), Some(candidate.change)))
                .collect();
            if let Some(listing) = listing.as_ref().filter(|_| query.scope == Scope::Tree) {
                candidates.extend(
                    listing
                        .paths
                        .iter()
                        .filter(|path| !changes.contains_key(path.as_str()))
                        .map(|path| (path.as_str(), None)),
                );
            }
            let (entries, truncated) = match_files(&candidates, &query.text, LIMIT);
            found.entries = entries;
            found.truncated = truncated;
        }
        Kind::Content if query.text.is_empty() => {}
        Kind::Content => {
            let ignore_case = !query.text.chars().any(char::is_uppercase);
            match query.scope {
                Scope::Changed => {
                    let files: Vec<(&str, Change, &FullText)> = query
                        .changed
                        .iter()
                        .filter_map(|candidate| {
                            Some((
                                candidate.path.as_str(),
                                candidate.change,
                                candidate.text.as_deref()?,
                            ))
                        })
                        .collect();
                    let (entries, truncated) =
                        grep_texts(&files, &query.text, ignore_case, LIMIT, &query.cancel);
                    found.entries = entries;
                    found.truncated = truncated;
                }
                Scope::Tree => match query.review.repo().grep(
                    &query.head,
                    &query.text,
                    ignore_case,
                    LIMIT + 1,
                    &query.cancel,
                ) {
                    Ok(lines) => {
                        found.truncated = lines.len() > LIMIT;
                        found.entries = lines
                            .into_iter()
                            .take(LIMIT)
                            .map(|hit| {
                                let change = changes.get(hit.path.as_str()).copied();
                                hit_entry(
                                    hit.path,
                                    hit.line,
                                    &hit.text,
                                    &query.text,
                                    ignore_case,
                                    change,
                                )
                            })
                            .collect();
                    }
                    Err(err) => found.error = Some(format!("git grep failed: {err}")),
                },
            }
        }
    }
    found
}

/// Fuzzy-matches `query` against the candidates' paths, best first; ties keep changed files
/// first, then the candidates' order. An empty query keeps every candidate in order.
#[must_use]
pub fn match_files(
    candidates: &[(&str, Option<Change>)],
    query: &str,
    limit: usize,
) -> (Vec<Entry>, bool) {
    let entry = |path: &str, change: Option<Change>, highlights: Vec<Range<usize>>| Entry {
        path: path.to_owned(),
        line: None,
        column: 0,
        text: String::new(),
        highlights,
        change,
    };
    if query.trim().is_empty() {
        let entries = candidates
            .iter()
            .take(limit)
            .map(|&(path, change)| entry(path, change, Vec::new()))
            .collect();
        return (entries, candidates.len() > limit);
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut chars = Vec::new();
    let mut indices = Vec::new();
    let mut scored: Vec<(u32, usize, Vec<Range<usize>>)> = Vec::new();
    for (position, &(path, _)) in candidates.iter().enumerate() {
        indices.clear();
        let Some(score) =
            pattern.indices(Utf32Str::new(path, &mut chars), &mut matcher, &mut indices)
        else {
            continue;
        };
        indices.sort_unstable();
        indices.dedup();
        scored.push((score, position, char_ranges(path, &indices)));
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| {
                let changed = |position: usize| {
                    candidates
                        .get(position)
                        .is_some_and(|(_, change)| change.is_some())
                };
                changed(b.1).cmp(&changed(a.1))
            })
            .then_with(|| a.1.cmp(&b.1))
    });
    let truncated = scored.len() > limit;
    let entries = scored
        .into_iter()
        .take(limit)
        .filter_map(|(_, position, highlights)| {
            let &(path, change) = candidates.get(position)?;
            Some(entry(path, change, highlights))
        })
        .collect();
    (entries, truncated)
}

/// The byte ranges of `text` covering the characters at `indices` (sorted), merged where they
/// touch.
fn char_ranges(text: &str, indices: &[u32]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut wanted = indices.iter().peekable();
    for (position, (start, character)) in text.char_indices().enumerate() {
        let Some(&&next) = wanted.peek() else {
            break;
        };
        if usize::try_from(next).ok() != Some(position) {
            continue;
        }
        wanted.next();
        let end = start + character.len_utf8();
        match ranges.last_mut() {
            Some(last) if last.end == start => last.end = end,
            _ => ranges.push(start..end),
        }
    }
    ranges
}

/// Every non-overlapping occurrence of `needle` in `text`, ignoring ASCII case when asked.
#[must_use]
pub fn occurrences(text: &str, needle: &str, ignore_case: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    if !ignore_case {
        return text
            .match_indices(needle)
            .map(|(start, found)| start..start + found.len())
            .collect();
    }
    super::find::ranges(text, needle)
}

/// An entry for a hit on zero-based line `line` of `path`, whose text is `text`.
fn hit_entry(
    path: String,
    line: usize,
    text: &str,
    needle: &str,
    ignore_case: bool,
    change: Option<Change>,
) -> Entry {
    let indent = text.len() - text.trim_start().len();
    let mut end = text.len().min(indent + ENTRY_TEXT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let shown = text.get(indent..end).unwrap_or_default().to_owned();
    let highlights = occurrences(&shown, needle, ignore_case);
    let column = occurrences(text, needle, ignore_case)
        .first()
        .map_or(0, |found| found.start);
    Entry {
        path,
        line: Some(line),
        column,
        text: shown,
        highlights,
        change,
    }
}

/// Every line of `files` that contains `needle`, in file and line order, up to `limit`; the
/// search stops early once `cancel` is set.
#[must_use]
pub fn grep_texts(
    files: &[(&str, Change, &FullText)],
    needle: &str,
    ignore_case: bool,
    limit: usize,
    cancel: &AtomicBool,
) -> (Vec<Entry>, bool) {
    let mut entries = Vec::new();
    let lowered = needle.to_ascii_lowercase();
    for &(path, change, text) in files {
        if cancel.load(Ordering::Relaxed) {
            return (Vec::new(), false);
        }
        for (line, content) in text.lines.iter().enumerate() {
            let hit = if ignore_case {
                content.to_ascii_lowercase().contains(&lowered)
            } else {
                content.contains(needle)
            };
            if !hit {
                continue;
            }
            if entries.len() == limit {
                return (entries, true);
            }
            entries.push(hit_entry(
                path.to_owned(),
                line,
                content,
                needle,
                ignore_case,
                Some(change),
            ));
        }
    }
    (entries, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In every built-in theme the picker's border stands out (3:1, as WCAG asks of a
    /// component's edge) from its surface and from the backdrop over the code, whose text
    /// shows through dimmed well below the picker's own.
    #[test]
    fn the_frame_stands_out_in_every_theme() {
        let (themes, problems) = crate::theme::all(None);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(themes.len(), 9);
        for theme in &themes {
            let colors = Palette::new(theme, None, None, crate::config::Background::Bold);
            let behind = crate::theme::mix(colors.text, colors.background, backdrop(&colors).a);
            for editing in [true, false] {
                let border = frame_border(&colors, editing);
                for (against, what) in
                    [(colors.surface, "surface"), (colors.background, "backdrop")]
                {
                    let ratio = crate::theme::contrast(border, against);
                    assert!(
                        ratio >= 3.0,
                        "{}: the border (editing {editing}) is {ratio:.2}:1 against the {what}",
                        theme.name
                    );
                }
            }
            let ratio = crate::theme::contrast(behind, colors.background);
            assert!(
                ratio < 1.6,
                "{}: code behind the backdrop still reads at {ratio:.2}:1",
                theme.name
            );
        }
    }

    /// A content hit as its line, its column, its text and its highlighted spans.
    type Hit<'a> = (usize, usize, &'a str, Vec<(usize, usize)>);

    /// Ranges as (start, end) pairs, which compare more readably.
    fn spans(ranges: &[Range<usize>]) -> Vec<(usize, usize)> {
        ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect()
    }

    fn change() -> Change {
        Change {
            letter: 'M',
            tone: FileTone::Modified,
            progress: Progress::Open,
            reviewed: 0,
            total: 1,
        }
    }

    /// The best fuzzy match comes first, with its matched characters highlighted, and a
    /// changed file wins a tie.
    #[test]
    fn files_match_fuzzily_best_first() {
        let candidates = [
            ("src/app/render.rs", None),
            ("docs/readme.md", None),
            ("src/rows.rs", Some(change())),
        ];
        let (entries, truncated) = match_files(&candidates, "rows", 10);
        assert!(!truncated);
        assert_eq!(
            entries.first().map(|entry| entry.path.as_str()),
            Some("src/rows.rs")
        );
        assert_eq!(
            entries.first().map(|entry| spans(&entry.highlights)),
            Some(vec![(4, 8)])
        );
        let (all, _) = match_files(&candidates, "", 2);
        assert_eq!(
            all.len(),
            2,
            "an empty query lists the candidates up to the limit"
        );
        let (none, _) = match_files(&candidates, "zzz", 10);
        assert_eq!(none, Vec::new());
    }

    /// Content hits are whole lines, trimmed of their indentation with the match moved along,
    /// in file order, and cut at the limit.
    #[test]
    fn content_hits_trim_and_highlight() {
        let text = FullText::new("fn main() {\n    let needle = 1;\n}\nNEEDLE\n");
        let cancel = AtomicBool::new(false);
        let files = [("a.rs", change(), &text)];
        let (entries, truncated) = grep_texts(&files, "needle", true, 10, &cancel);
        assert!(!truncated);
        let found: Vec<Hit<'_>> = entries
            .iter()
            .map(|entry| {
                (
                    entry.line.unwrap(),
                    entry.column,
                    entry.text.as_str(),
                    spans(&entry.highlights),
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                (1, 8, "let needle = 1;", vec![(4, 10)]),
                (3, 0, "NEEDLE", vec![(0, 6)])
            ],
            "the column is the match's in the full line, before trimming"
        );
        let (exact, _) = grep_texts(&files, "NEEDLE", false, 10, &cancel);
        assert_eq!(exact.len(), 1, "an upper-case query matches case");
        let (cut, truncated) = grep_texts(&files, "needle", true, 1, &cancel);
        assert_eq!((cut.len(), truncated), (1, true));
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            grep_texts(&files, "needle", true, 10, &cancel).0,
            Vec::new()
        );
    }

    #[test]
    fn character_indices_become_byte_ranges() {
        assert_eq!(spans(&char_ranges("aébc", &[1, 2, 3])), [(1, 5)]);
        assert_eq!(spans(&char_ranges("abc", &[0, 2])), [(0, 1), (2, 3)]);
    }
}
