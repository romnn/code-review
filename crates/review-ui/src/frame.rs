//! What a frame is laid out in, and where each scrolled list is: the geometry a front end
//! measures, the rows and cells the model works out from it, and the lists' offsets, which the
//! model owns in rows.
//!
//! Each frame, a front end measures its lists once and hands the model the [`Geometry`], plain
//! data in its own units, with the frame's layout phase.
//! The model keeps the last one and works out the [`Grid`], rooms and pan extents from it, in
//! the [`Sizes`] its [`Scale`] gives at the code size it needs.
//! A code size the front end has not laid out yet is worked out the same way, from how the bars
//! around the lists change with it, so a transition keeps the reviewer's place before the next
//! frame.
//!
//! A front end mirrors each list's offset into its own scrolling every frame and turns its input
//! (the wheel, scrollbar drags and track clicks) into calls on [`ListScroll`], so nothing scrolls
//! a list behind the model's back.
//! Scrolls that need the list's real viewport wait as a `Request` until the frame's layout
//! phase, which resolves them against the frame's [`Grid`]; a list not laid out yet keeps its
//! request until it is.

use std::time::Instant;

use crate::config::Density;
use crate::scroll::{self, Animation};

/// A scrolled list the model drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    /// The selected file's diff.
    Diff,
    /// The whole file as a buffer.
    Buffer,
    /// The file tree.
    Tree,
    /// The file finder's or the content search's entries.
    Picker,
    /// The settings list.
    Settings,
}

impl List {
    /// Every list, in the order a frame lays them out.
    pub const ALL: [Self; 5] = [
        Self::Diff,
        Self::Buffer,
        Self::Tree,
        Self::Picker,
        Self::Settings,
    ];
}

/// The part of the model's state that the geometry depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// The code size, which every size in the window follows.
    pub code_size: f32,
    /// The shown look's density, which sets the row height.
    pub density: Density,
    /// While the settings list is open, the code size it takes its own text size from.
    pub panel: Option<f32>,
}

/// One frame as the front end laid it out, in its units: logical pixels in a window, cells in
/// a terminal.
///
/// The front end measures it once per frame and hands it to the model's layout phase
/// ([`crate::model::Model::layout`]); the model works out everything else from it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Geometry {
    /// The code size the lists were laid out at, which their heights reflect.
    pub laid_out: f32,
    /// The diff pane's width, which decides whether side by side fits.
    pub diff_width: f32,
    /// The diff list's width, 0 before its first layout.
    pub diff_list: f32,
    /// The narrower half of the diff list's side-by-side rows, gutter and sign included.
    pub diff_half: f32,
    /// The buffer list's width, 0 before its first layout.
    pub buffer_list: f32,
    /// Each list's height.
    pub heights: Heights,
}

/// Each list's height as last laid out, in the front end's units, `None` before its first
/// layout.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Heights {
    /// The diff.
    pub diff: Option<f32>,
    /// The buffer.
    pub buffer: Option<f32>,
    /// The file tree.
    pub tree: Option<f32>,
    /// The picker's entries.
    pub picker: Option<f32>,
    /// The settings list.
    pub settings: Option<f32>,
}

/// How large a front end draws a frame at one code size, density and settings text size, in
/// its units: what turns a [`Geometry`] into rows and cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sizes {
    /// A code row's height.
    pub row_height: f32,
    /// A cell of code.
    pub code_cell: f32,
    /// A cell of the text of the tool's own rows, such as hunk headers and notes.
    pub text_cell: f32,
    /// The bar along a row's left edge.
    pub frame: f32,
    /// The line between the halves of a side-by-side diff.
    pub divider: f32,
    /// A code row's line-number gutter.
    pub gutter: f32,
    /// A code row's sign column.
    pub sign: f32,
    /// The change marker along the left edge of a buffer's line.
    pub marker: f32,
    /// The room after a buffer's line numbers, in code cells.
    pub number_room: f32,
    /// The room past the widest half that the side-by-side code pans to.
    pub half_slack: f32,
    /// What one of the tool's own rows adds past its text and the start of code: its padding
    /// and the gaps between its parts.
    pub note_spacing: f32,
    /// How much of the page before a track click's page keeps in view.
    pub page_context: f32,
    /// The bars around each list, which follow the code size.
    pub chrome: Chrome,
    /// A row of the settings list, which keeps the text size it opened with.
    pub panel_row_height: f32,
}

/// The height of the bars above and below each list, in the front end's units.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Chrome {
    /// Around the diff and around the buffer.
    pub code: f32,
    /// Around the file tree.
    pub tree: f32,
    /// Around the picker's entries.
    pub picker: f32,
    /// Around the settings list.
    pub settings: f32,
}

/// How a front end's [`Sizes`] follow the [`Metrics`]: a pure function the front end hands
/// the model once.
///
/// It reads nothing of the front end's state, so the model can work out the geometry of a code
/// size the front end has not laid out yet without asking it.
pub struct Scale(Box<dyn Fn(&Metrics) -> Sizes>);

impl Scale {
    /// The sizes `sizes` gives at each metrics.
    #[must_use]
    pub fn new(sizes: impl Fn(&Metrics) -> Sizes + 'static) -> Self {
        Self(Box::new(sizes))
    }

    /// The same sizes at every metrics, as a terminal's cells are.
    #[must_use]
    pub fn fixed(sizes: Sizes) -> Self {
        Self::new(move |_| sizes)
    }

    /// The sizes at `metrics`.
    #[must_use]
    pub fn sizes(&self, metrics: &Metrics) -> Sizes {
        (self.0)(metrics)
    }
}

/// The width of a buffer's line numbers for a file `lines` lines long, in code cells
/// `code_cell` wide: the last line's number, four digits at least, and `room` cells after it.
#[must_use]
pub fn number_column(code_cell: f32, room: f32, lines: usize) -> f32 {
    let digits = lines.to_string().len().max(4);
    (code_cell * (scroll::float(digits) + room)).round()
}

/// A height in rows `row_height` tall.
fn in_rows(height: f32, row_height: f32) -> f64 {
    f64::from(height) / f64::from(row_height)
}

impl Geometry {
    /// The grid at `metrics`, in the sizes `scale` gives.
    ///
    /// At the code size the lists were laid out at, the viewports are as laid out; at another,
    /// each grows by as much as the bars around it shrink, so a transition to a new code size
    /// keeps the reviewer's place against the viewports the next frame will have.
    #[must_use]
    pub fn grid(&self, scale: &Scale, metrics: &Metrics) -> Grid {
        let sizes = scale.sizes(metrics);
        let before = scale.sizes(&Metrics {
            code_size: self.laid_out,
            ..*metrics
        });
        Grid {
            row_height: sizes.row_height,
            code_cell: sizes.code_cell,
            half_cells: self.half_cells(&sizes),
            page_context: in_rows(sizes.page_context, sizes.row_height),
            viewports: self.viewports(&before, &sizes, metrics),
        }
    }

    /// The cells of code each half of a side-by-side diff across the pane has, after the
    /// frame bar, the divider and each half's gutter and sign.
    fn half_cells(&self, sizes: &Sizes) -> f32 {
        let half =
            (self.diff_width - sizes.frame - sizes.divider) / 2.0 - sizes.gutter - sizes.sign;
        half / sizes.code_cell
    }

    /// The viewports in rows of the lists, laid out in the sizes `before`, at the sizes
    /// `after` of `metrics`, with the settings list's while it is open.
    fn viewports(&self, before: &Sizes, after: &Sizes, metrics: &Metrics) -> Viewports {
        #[expect(
            clippy::float_cmp,
            reason = "a code size compared with itself, unchanged"
        )]
        let same = self.laid_out == metrics.code_size;
        let grown = |height: f32, chrome: fn(&Chrome) -> f32| {
            if same {
                height
            } else {
                height + (chrome(&before.chrome) - chrome(&after.chrome))
            }
        };
        let row_height = after.row_height;
        let code_rows = |height: Option<f32>, chrome: fn(&Chrome) -> f32| {
            height.map(|height| in_rows(grown(height, chrome), row_height))
        };
        let Heights {
            diff,
            buffer,
            tree,
            picker,
            settings,
        } = self.heights;
        Viewports {
            diff: code_rows(diff, |chrome| chrome.code),
            buffer: code_rows(buffer, |chrome| chrome.code),
            tree: tree.map(|height| {
                // Added and taken apart, in the order the tree's place was always kept in.
                let height = if same {
                    height
                } else {
                    height + before.chrome.tree - after.chrome.tree
                };
                in_rows(height, row_height)
            }),
            picker: code_rows(picker, |chrome| chrome.picker),
            settings: metrics.panel.and(settings).map(|height| {
                in_rows(
                    grown(height, |chrome| chrome.settings),
                    after.panel_row_height,
                )
            }),
        }
    }

    /// How far the side-by-side diff's code can move left, in code cells, when its widest half
    /// is `widest_half` cells wide and its widest full-width line of code `widest_code` cells.
    #[must_use]
    pub(crate) fn text_extent(
        &self,
        sizes: &Sizes,
        widest_half: usize,
        widest_code: Option<usize>,
    ) -> f64 {
        let code_cell = sizes.code_cell;
        let columns = sizes.gutter + sizes.sign;
        let half = self.diff_half - columns;
        let full = self.diff_list - (sizes.frame + columns);
        let overflow = |cells: usize, room: f32| scroll::float(cells) * code_cell - room;
        let halves = overflow(widest_half, half);
        let code = widest_code.map_or(0.0, |cells| overflow(cells, full));
        // A little room past the longest line, as an editor leaves it.
        let extent = (halves.max(code) + sizes.half_slack).max(0.0);
        f64::from(extent) / f64::from(code_cell)
    }

    /// How far the diff's code must move left, in code cells, for the end of one of the
    /// tool's own rows `cells` cells wide (as [`crate::rows::Note::width`] counts them) to
    /// show: at least how far its text runs past its room, none when it fits.
    ///
    /// The code pans this far, so every row's end can show; each row itself moves only by
    /// what it really runs past, [`Self::reach_overflow`].
    #[must_use]
    pub(crate) fn annotation_overflow(&self, sizes: &Sizes, cells: usize) -> f64 {
        let reach = scroll::float(cells) * sizes.text_cell + sizes.note_spacing;
        self.reach_overflow(sizes, reach)
    }

    /// How far, in code cells, one of the tool's own rows runs past the diff's right edge when
    /// what it draws reaches `reach` past where code starts, in the units of `sizes`; none when
    /// it fits.
    #[must_use]
    pub(crate) fn reach_overflow(&self, sizes: &Sizes, reach: f32) -> f64 {
        // Before the list's first layout there is no room to measure.
        if self.diff_list <= 0.0 {
            return 0.0;
        }
        let start = sizes.frame + sizes.gutter + sizes.sign;
        f64::from((start + reach - self.diff_list).max(0.0)) / f64::from(sizes.code_cell)
    }

    /// The code cells a line across the diff's width has room for, after its frame bar,
    /// gutter and sign.
    #[must_use]
    pub(crate) fn code_room(&self, sizes: &Sizes) -> f64 {
        let room = self.diff_list - sizes.frame - sizes.gutter - sizes.sign;
        f64::from(room.max(0.0)) / f64::from(sizes.code_cell)
    }

    /// Whether the buffer list has been laid out, so its room is known.
    #[must_use]
    pub(crate) fn buffer_measured(&self) -> bool {
        self.buffer_list > 0.0
    }

    /// The code cells a line of the buffer has room for, after its change marker and the line
    /// numbers of a file `lines` lines long.
    #[must_use]
    pub(crate) fn buffer_room(&self, sizes: &Sizes, lines: usize) -> f64 {
        let numbers = number_column(sizes.code_cell, sizes.number_room, lines);
        let room = self.buffer_list - sizes.marker - numbers;
        f64::from(room.max(0.0)) / f64::from(sizes.code_cell)
    }
}

/// One frame in the model's units, rows and code cells, as the model works it out from a
/// [`Geometry`] at its metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    /// A code row's height in the front end's units: logical pixels in a window, 1 in a
    /// terminal.
    pub row_height: f32,
    /// A code cell's width in the same units.
    pub code_cell: f32,
    /// The cells of code each half of a side-by-side diff has room for.
    pub half_cells: f32,
    /// The rows a track click's page keeps in view from the page before.
    pub page_context: f64,
    /// Each list's viewport.
    pub viewports: Viewports,
}

/// Each list's viewport height in its own rows, `None` while the list has not been laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Viewports {
    /// The diff, in code rows.
    pub diff: Option<f64>,
    /// The buffer, in code rows.
    pub buffer: Option<f64>,
    /// The file tree, in code rows.
    pub tree: Option<f64>,
    /// The picker's entries, in code rows.
    pub picker: Option<f64>,
    /// The settings list, in the panel's own rows; `None` while it is closed.
    pub settings: Option<f64>,
}

impl Viewports {
    /// The viewport of `list`.
    #[must_use]
    pub(crate) fn of(&self, list: List) -> Option<f64> {
        match list {
            List::Diff => self.diff,
            List::Buffer => self.buffer,
            List::Tree => self.tree,
            List::Picker => self.picker,
            List::Settings => self.settings,
        }
    }
}

/// A scroll that waits for its list's viewport, resolved in the layout phase.
///
/// A list not shown keeps its request until it shows; one shown without rows drops it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    /// Puts `row` at the top, when any of it is out of view.
    Top(usize),
    /// Centers `row`, wherever it is.
    Center(usize),
    /// Brings `row` into view by the least scroll: at the top when it is above, at the bottom
    /// when it is below.
    Nearest(usize),
}

/// The most a list of `count` rows scrolls in a viewport `viewport` rows tall; a list not laid
/// out yet has no height.
#[must_use]
pub(crate) fn max_offset(count: usize, viewport: Option<f64>) -> f64 {
    (scroll::rows(count) - viewport.unwrap_or(0.0)).max(0.0)
}

/// `offset` kept within a list of `count` rows in a viewport `viewport` rows tall.
#[must_use]
pub(crate) fn clamp(offset: f64, count: usize, viewport: Option<f64>) -> f64 {
    within(offset, max_offset(count, viewport))
}

/// `offset` kept within `0..=max`.
fn within(offset: f64, max: f64) -> f64 {
    // Never a negative zero, which front ends that report pixels would print as `-0.0`.
    if offset > 0.0 { offset.min(max) } else { 0.0 }
}

/// Where one list is: its offset in rows, the eased scroll moving it, and what waits for its
/// next layout.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ListScroll {
    /// Rows scrolled past the top; the one place a list's position lives.
    offset: f64,
    animation: Option<Animation>,
    request: Option<Request>,
    /// A track click's page, the offset it eases to.
    page: Option<f64>,
}

impl ListScroll {
    /// Rows scrolled past the top now.
    #[must_use]
    pub fn offset(&self) -> f64 {
        self.offset
    }

    /// Where the list is, or where its eased scroll is heading.
    #[must_use]
    pub(crate) fn target(&self) -> f64 {
        self.animation
            .map_or(self.offset, |animation| animation.target())
    }

    /// Whether an eased scroll is moving the list.
    #[must_use]
    pub fn animating(&self) -> bool {
        self.animation.is_some()
    }

    /// Puts the list at `offset` at once; the layout phase keeps it within the list.
    pub fn jump(&mut self, offset: f64) {
        self.offset = offset;
    }

    /// Puts the list at `offset` at once, within the list.
    pub(crate) fn scroll_to(&mut self, offset: f64, count: usize, viewport: Option<f64>) {
        self.offset = clamp(offset, count, viewport);
    }

    /// Moves the list by `delta` rows at once, within the list.
    pub(crate) fn scroll_by(&mut self, delta: f64, count: usize, viewport: Option<f64>) {
        self.scroll_to(self.offset + delta, count, viewport);
    }

    /// Starts or retargets an eased scroll to `target`, within the list.
    ///
    /// The bound comes from the row count, so it holds right after the rows change, before
    /// they are laid out.
    pub(crate) fn ease_to(
        &mut self,
        target: f64,
        count: usize,
        viewport: Option<f64>,
        now: Instant,
    ) {
        let target = clamp(target, count, viewport);
        match &mut self.animation {
            Some(animation) => animation.retarget(target, now),
            None => self.animation = Some(Animation::new(self.offset, target, now)),
        }
    }

    /// Stops the eased scroll and drops a track click's page that has not started, as when the
    /// wheel takes over or the rows change under them.
    pub fn stop(&mut self) {
        self.animation = None;
        self.page = None;
    }

    /// Asks for `request` once the list is laid out, replacing an earlier one.
    pub(crate) fn request(&mut self, request: Request) {
        self.request = Some(request);
    }

    /// A track click toward `toward`: one page that way, keeping `context` rows of the page
    /// before, eased to in the next layout.
    pub(crate) fn page_toward(
        &mut self,
        toward: f64,
        count: usize,
        viewport: Option<f64>,
        context: f64,
    ) {
        let height = viewport.unwrap_or(0.0);
        let page = (height - context).max(context);
        let target = if toward > self.offset {
            self.offset + page
        } else {
            self.offset - page
        };
        self.page = Some(clamp(target, count, viewport));
    }

    /// The layout phase of a list of `count` rows in a viewport `viewport` rows tall: starts a
    /// waiting page, steps the eased scroll, and, when the list shows this frame, keeps the
    /// offset within the list and resolves the waiting request.
    ///
    /// Returns whether the eased scroll still runs.
    pub(crate) fn lay_out(
        &mut self,
        count: usize,
        viewport: Option<f64>,
        shown: bool,
        now: Instant,
    ) -> bool {
        if let Some(target) = self.page.take() {
            self.ease_to(target, count, viewport, now);
        }
        let running = match self.animation {
            Some(animation) => {
                self.offset = animation.value_at(now);
                if animation.done(now) {
                    self.animation = None;
                }
                self.animation.is_some()
            }
            None => false,
        };
        if shown {
            if let Some(viewport) = viewport {
                self.offset = clamp(self.offset, count, Some(viewport));
            }
            self.resolve(count, viewport);
        }
        running
    }

    /// Resolves the waiting request against a shown list: drops it when the list has no rows,
    /// keeps it while the list has no viewport yet.
    fn resolve(&mut self, count: usize, viewport: Option<f64>) {
        let Some(request) = self.request else {
            return;
        };
        if count == 0 {
            self.request = None;
            return;
        }
        let Some(height) = viewport else {
            return;
        };
        self.request = None;
        let max = max_offset(count, viewport);
        let above = |row: usize| scroll::rows(row) < self.offset;
        let below = |row: usize| scroll::rows(row) + 1.0 > self.offset + height;
        let offset = match request {
            Request::Top(row) => (above(row) || below(row)).then(|| scroll::rows(row)),
            Request::Center(row) => Some(scroll::rows(row) + 0.5 - height / 2.0),
            Request::Nearest(row) => {
                if above(row) {
                    Some(scroll::rows(row))
                } else if below(row) {
                    Some(scroll::rows(row) + 1.0 - height)
                } else {
                    None
                }
            }
        };
        if let Some(offset) = offset {
            self.offset = within(offset, max);
        }
    }
}

/// The room past the longest line of code that panning leaves, in code cells, as an editor
/// leaves some.
pub const PAN_SLACK: f64 = 2.0;

/// How far code `widest` cells wide can move left in `room` cells: past its end by
/// [`PAN_SLACK`], or not at all when it fits.
#[must_use]
pub fn pan_extent(widest: usize, room: f64) -> f64 {
    let overflow = scroll::rows(widest) - room;
    if overflow > 0.0 {
        overflow + PAN_SLACK
    } else {
        0.0
    }
}

/// A horizontal offset of code, in code cells: how far the code of every line has moved left,
/// within how far it can, while gutters and the tool's own rows stay put.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TextPan {
    offset: f64,
    extent: f64,
}

impl TextPan {
    /// How far the code has moved left.
    #[must_use]
    pub fn offset(&self) -> f64 {
        self.offset
    }

    /// How far the code can move.
    #[must_use]
    pub fn extent(&self) -> f64 {
        self.extent
    }

    /// Moves the code by `delta` cells, within the extent.
    pub(crate) fn pan(&mut self, delta: f64) {
        self.offset = within(self.offset + delta, self.extent);
    }

    /// Moves the code to `offset` cells, within the extent.
    pub(crate) fn set(&mut self, offset: f64) {
        self.offset = within(offset, self.extent);
    }

    /// Takes the extent the rows have now, keeping the offset within it.
    pub(crate) fn set_extent(&mut self, extent: f64) {
        self.extent = extent.max(0.0);
        self.clamp_to_extent();
    }

    /// Moves the code back to the left edge, as when another file shows.
    pub(crate) fn reset(&mut self) {
        self.offset = 0.0;
    }

    /// Keeps the offset within the extent, as after the rows or their metrics changed.
    pub(crate) fn clamp_to_extent(&mut self) {
        self.offset = within(self.offset, self.extent);
    }

    /// Moves the code the least that shows cell `cell` in `room` cells with `margin` cells
    /// beside it where the line has them, as an editor follows its cursor sideways.
    pub(crate) fn reveal(&mut self, cell: usize, room: f64, margin: f64) {
        let cell = scroll::rows(cell);
        // A room too narrow for both margins keeps the cell in its middle.
        let margin = margin.min(((room - 1.0) / 2.0).max(0.0));
        if cell < self.offset + margin {
            self.set(cell - margin);
        } else if cell + 1.0 > self.offset + room - margin {
            self.set(cell + 1.0 + margin - room);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A list scrolled `offset` rows down.
    fn at(offset: f64) -> ListScroll {
        ListScroll {
            offset,
            ..ListScroll::default()
        }
    }

    /// A request waits while its list has no viewport, and resolves in the first layout that
    /// has one, against the offset of that moment.
    #[test]
    fn a_request_waits_for_the_first_layout() {
        let now = Instant::now();
        let mut list = ListScroll::default();
        list.request(Request::Center(50));

        // Shown but not laid out: nothing to center in yet.
        list.lay_out(100, None, true, now);
        assert_eq!(list.offset(), 0.0);
        assert_eq!(list.request, Some(Request::Center(50)));

        // Laid out but not shown: a hidden list keeps its request too.
        list.lay_out(100, Some(20.0), false, now);
        assert_eq!(list.request, Some(Request::Center(50)));

        // The first layout with a viewport centers row 50: its middle, 50.5, at 10 rows down.
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 40.5);
        assert_eq!(list.request, None);
    }

    /// A shown list without rows drops its request, as there is nothing to scroll to.
    #[test]
    fn an_empty_list_drops_its_request() {
        let mut list = ListScroll::default();
        list.request(Request::Top(3));
        list.lay_out(0, None, true, Instant::now());
        assert_eq!(list.request, None);
    }

    /// Centering stays within the list at both ends.
    #[test]
    fn centering_clamps_to_the_list() {
        let now = Instant::now();
        let mut list = at(30.0);
        list.request(Request::Center(2));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 0.0, "near the top");
        list.request(Request::Center(98));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 80.0, "near the bottom");
    }

    /// `Top` moves a row that is out of view to the top, from above or below, and leaves a
    /// visible one alone.
    #[test]
    fn top_puts_a_hidden_row_at_the_top() {
        let now = Instant::now();
        let mut list = at(10.0);
        list.request(Request::Top(15));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 10.0, "visible");
        list.request(Request::Top(30));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 30.0, "below, still to the top");
        list.request(Request::Top(5));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 5.0, "above");
        list.request(Request::Top(95));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 80.0, "within the list");
    }

    /// `Nearest` scrolls the least: a row below lands at the bottom edge, one above at the top.
    #[test]
    fn nearest_scrolls_the_least() {
        let now = Instant::now();
        let mut list = at(10.0);
        list.request(Request::Nearest(29));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 10.0, "the last whole row is visible");
        list.request(Request::Nearest(30));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 11.0, "one row below: one row down");
        list.request(Request::Nearest(4));
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.offset(), 4.0, "above: to the top");
    }

    /// The layout phase keeps a shown list's offset within its rows after they shrink or the
    /// viewport grows, but leaves a hidden list for when it shows, as its viewport may differ.
    #[test]
    fn layout_clamps_a_shown_list_after_a_shrink() {
        let now = Instant::now();
        let mut list = at(70.0);
        list.lay_out(50, Some(20.0), false, now);
        assert_eq!(list.offset(), 70.0, "hidden");
        list.lay_out(50, None, true, now);
        assert_eq!(list.offset(), 70.0, "no viewport to clamp against yet");
        list.lay_out(50, Some(20.0), true, now);
        assert_eq!(list.offset(), 30.0);
        list.lay_out(50, Some(60.0), true, now);
        assert_eq!(list.offset(), 0.0, "everything fits");
    }

    /// The wheel and drags stay within the list at once, not only at the next layout.
    #[test]
    fn scrolling_by_stays_within_the_list() {
        let mut list = ListScroll::default();
        list.scroll_by(-3.0, 100, Some(20.0));
        assert_eq!(list.offset(), 0.0);
        list.scroll_by(500.0, 100, Some(20.0));
        assert_eq!(list.offset(), 80.0);
    }

    /// An eased scroll runs through the layouts of later frames and lands on its target; its
    /// target is where the list is heading in the meantime.
    #[test]
    fn eased_scrolls_land_on_their_target() {
        let now = Instant::now();
        let mut list = ListScroll::default();
        list.ease_to(500.0, 100, Some(20.0), now);
        assert_eq!(list.target(), 80.0, "clamped to the list");
        assert!(list.lay_out(100, Some(20.0), true, now + Duration::from_millis(50)));
        assert!(list.offset() > 0.0 && list.offset() < 80.0);
        assert!(!list.lay_out(100, Some(20.0), true, now + scroll::DURATION));
        assert_eq!(list.offset(), 80.0);
        assert!(!list.animating());
    }

    /// A track click pages toward the click, keeping some context, and starts easing only in
    /// the next layout; stopping drops a page that has not started.
    #[test]
    fn track_clicks_page_toward_the_click() {
        let now = Instant::now();
        let mut list = at(10.0);
        list.page_toward(50.0, 100, Some(20.0), 1.5);
        assert_eq!(list.target(), 10.0, "waits for the layout");
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.target(), 28.5);
        let mut list = at(10.0);
        list.page_toward(0.0, 100, Some(20.0), 1.5);
        list.stop();
        list.lay_out(100, Some(20.0), true, now);
        assert_eq!(list.target(), 10.0, "stopped before it started");
    }

    /// Code pans only as far as it runs past its room, then a little more; code that fits
    /// does not pan at all.
    #[test]
    fn pans_reach_past_the_longest_line() {
        assert_eq!(pan_extent(90, 60.0), 30.0 + PAN_SLACK);
        assert_eq!(pan_extent(60, 60.0), 0.0);
        assert_eq!(pan_extent(10, 60.0), 0.0);
    }

    /// Following a cell moves the code the least that shows it with its margin: right when
    /// the cell runs past the room's end, left when it is before its start, and not at all
    /// when it shows; a room too narrow for both margins keeps the cell in its middle.
    #[test]
    fn following_a_cell_pans_the_least() {
        let mut text = TextPan::default();
        text.set_extent(100.0);
        text.reveal(10, 40.0, 4.0);
        assert_eq!(text.offset(), 0.0, "in view");
        text.reveal(50, 40.0, 4.0);
        assert_eq!(
            text.offset(),
            15.0,
            "past the end: cell 50 and four after it show"
        );
        text.reveal(16, 40.0, 4.0);
        assert_eq!(
            text.offset(),
            12.0,
            "before the start: four cells before cell 16"
        );
        text.reveal(2, 40.0, 4.0);
        assert_eq!(text.offset(), 0.0, "never before the line's start");
        text.reveal(30, 5.0, 4.0);
        assert_eq!(text.offset(), 28.0, "a narrow room centers the cell");
    }

    /// The side-by-side text stays within its extent, which follows the rows.
    #[test]
    fn text_pans_within_its_extent() {
        let mut text = TextPan::default();
        text.set_extent(12.0);
        text.pan(20.0);
        assert_eq!(text.offset(), 12.0);
        text.set_extent(5.0);
        assert_eq!(text.offset(), 5.0);
        text.pan(-8.0);
        assert_eq!(text.offset(), 0.0);
    }
}
