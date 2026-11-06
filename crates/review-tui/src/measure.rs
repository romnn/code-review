//! Where everything goes on the terminal, in cells, and the model's geometry of it.
//!
//! The screen is review-gui's window on the terminal's grid: the files pane at the left, a
//! one-cell divider, the diff with its header at the right, then the `/` bar while it takes
//! typing, the status line and the hints line.
//! Every row is one cell tall and every column one cell wide, so the model's geometry is plain:
//! rows of height 1, code cells of width 1, and the heights the panes have in rows, the same at
//! every code size.

use ratatui::layout::Rect;
use review_ui::frame::{Chrome, Geometry, Heights, Sizes};
use review_ui::model::{Mode, Model, PaneLayout};
use review_ui::paint::settings;
use review_ui::panes;

/// The bar along a row's left edge, in cells.
pub const FRAME: u16 = 1;

/// A code row's line-number gutter, in cells: four digits and the room after them.
pub const GUTTER: u16 = 5;

/// A code row's sign column, in cells: the sign and a space.
pub const SIGN: u16 = 2;

/// The line between the halves of the side-by-side diff, in cells.
pub const DIVIDER: u16 = 1;

/// The change marker along the left edge of a buffer's line, in cells.
pub const MARKER: u16 = 1;

/// The line numbers' column of a buffer `lines` lines long, in cells: the last line's number,
/// four digits at least, and a cell of room.
#[must_use]
pub fn buffer_numbers(lines: usize) -> u16 {
    let digits = u16::try_from(lines.to_string().len()).unwrap_or(u16::MAX);
    digits.max(4).saturating_add(1)
}

/// Room left past the longest line of code when the side-by-side text pans, in cells.
const PAN_ROOM: f32 = 2.0;

/// The files pane: its header, its two progress lines, the rule under them and the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sidebar {
    /// The whole pane.
    pub area: Rect,
    /// `Files 17` and the filter.
    pub header: Rect,
    /// The files and hunks progress lines.
    pub progress: Rect,
    /// The rule between the progress and the tree.
    pub rule: Rect,
    /// The tree's rows.
    pub tree: Rect,
}

/// Where everything goes on one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    /// The whole screen.
    pub area: Rect,
    /// The files pane, while it shows.
    pub sidebar: Option<Sidebar>,
    /// The divider right of the files pane, while it shows.
    pub divider: Option<Rect>,
    /// The diff's or the buffer's pane: its header and its list.
    pub main: Rect,
    /// The file header over the diff.
    pub header: Rect,
    /// The diff's or the buffer's rows.
    pub list: Rect,
    /// The `/` bar, while it takes typing.
    pub find: Option<Rect>,
    /// The status line.
    pub status: Rect,
    /// What the keys act on and the keys' hints.
    pub hints: Rect,
    /// The open picker.
    pub picker: Option<PickerPanes>,
    /// The open settings list.
    pub settings: Option<SettingsPanes>,
}

/// An open picker: a box in the upper middle of the screen with its query, its entries, the
/// content search's preview beside them, and a footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerPanes {
    /// The box with its border.
    pub area: Rect,
    /// The query line.
    pub query: Rect,
    /// The entries.
    pub list: Rect,
    /// The content search's preview, right of the entries.
    pub preview: Option<Rect>,
    /// What the keys do now.
    pub footer: Rect,
}

/// The settings list, in the files pane's place, or over the left of the diff when the files
/// pane is hidden or too narrow, so the diff keeps its geometry for previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsPanes {
    /// The whole panel.
    pub area: Rect,
    /// `Settings`.
    pub title: Rect,
    /// The search field and how many entries show.
    pub search: Rect,
    /// The rule under the search, accented while it has the keys.
    pub rule: Rect,
    /// The entries.
    pub list: Rect,
    /// The rule over the footer.
    pub footer_rule: Rect,
    /// What the cursor's entry does.
    pub footer: Rect,
    /// The hairline along the panel's right edge over the diff; in the files pane's place, the
    /// pane's own divider is there.
    pub edge: Option<Rect>,
}

/// The widest picker, in cells.
const PICKER_CELLS: u16 = 120;

/// The rows between the top of the screen and a picker.
const PICKER_TOP: u16 = 2;

impl Panes {
    /// The panes of `model` on a screen `area` large.
    #[must_use]
    pub fn new(area: Rect, model: &Model) -> Self {
        let mut panes = Self::lay_out(area, model.layout, model.sidebar_shown(), model.typing());
        panes.picker = model.picker.as_ref().map(|picker| {
            PickerPanes::lay_out(area, picker.kind == review_ui::model::picker::Kind::Content)
        });
        panes.settings = model
            .settings_panel
            .is_some()
            .then(|| SettingsPanes::lay_out(&panes));
        panes
    }

    /// The panes on a screen `area` large with the files pane `layout` wide when `sidebar`
    /// shows, and the `/` bar when `find` shows.
    #[must_use]
    pub fn lay_out(area: Rect, layout: PaneLayout, sidebar: bool, find: bool) -> Self {
        let bottom = area.bottom();
        let hints = row(area, bottom.saturating_sub(1));
        let status = row(area, bottom.saturating_sub(2));
        let find = find.then(|| row(area, bottom.saturating_sub(3)));
        let bars = 2 + u16::from(find.is_some());
        let top = Rect {
            height: area.height.saturating_sub(bars),
            ..area
        };
        let (sidebar, divider, main) = if sidebar {
            let width = panes::sidebar_cells(layout.sidebar, top.width);
            let side = Rect { width, ..top };
            let divider = Rect {
                x: top.x + width,
                width: DIVIDER.min(top.width.saturating_sub(width)),
                ..top
            };
            let main = Rect {
                x: divider.right(),
                width: top.right().saturating_sub(divider.right()),
                ..top
            };
            (Some(Sidebar::of(side)), Some(divider), main)
        } else {
            (None, None, top)
        };
        let header = Rect {
            height: main.height.min(1),
            ..main
        };
        let list = Rect {
            y: header.bottom(),
            height: main.height.saturating_sub(header.height),
            ..main
        };
        Self {
            area,
            sidebar,
            divider,
            main,
            header,
            list,
            find,
            status,
            hints,
            picker: None,
            settings: None,
        }
    }
}

impl PickerPanes {
    /// A picker on a screen `screen` large, with a preview beside its entries when `preview`.
    #[must_use]
    pub fn lay_out(screen: Rect, preview: bool) -> Self {
        let width = PICKER_CELLS.min(share(screen.width, 94));
        let height = share(screen.height, 75);
        let top = PICKER_TOP.min(screen.height.saturating_sub(height));
        let area = Rect {
            x: screen.x + screen.width.saturating_sub(width) / 2,
            y: screen.y + top,
            width,
            height,
        };
        // Inside the border: the query, a rule, the entries, a rule and the footer.
        let inner = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            ..area
        };
        let query = row(inner, area.y + 1);
        let footer = row(inner, area.bottom().saturating_sub(2));
        let list = Rect {
            y: area.y + 3,
            height: area.height.saturating_sub(6),
            ..inner
        };
        let (list, preview) = if preview {
            let left = list.width / 2;
            let preview = Rect {
                x: list.x + left + 1,
                width: list.width.saturating_sub(left + 1),
                ..list
            };
            (
                Rect {
                    width: left,
                    ..list
                },
                Some(preview),
            )
        } else {
            (list, None)
        };
        Self {
            area,
            query,
            list,
            preview,
            footer,
        }
    }
}

impl SettingsPanes {
    /// The settings panel beside the frame's other `panes`.
    #[must_use]
    pub fn lay_out(panes: &Panes) -> Self {
        let screen = panes.area;
        let rows = panes.main.height;
        let in_files = panes
            .sidebar
            .filter(|sidebar| f32::from(sidebar.area.width) >= settings::PANEL_MIN_CELLS);
        let (area, edge) = if let Some(sidebar) = in_files {
            (sidebar.area, None)
        } else {
            let max = f32::from(screen.width) * settings::PANEL_MAX_SHARE;
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a share of a terminal's width in cells, within u16"
            )]
            let width = settings::PANEL_CELLS.min(max).floor() as u16;
            let area = Rect {
                x: screen.x,
                y: screen.y,
                width,
                height: rows,
            };
            let edge = Rect {
                x: area.right().saturating_sub(1),
                width: area.width.min(1),
                ..area
            };
            let content = Rect {
                width: area.width.saturating_sub(1),
                ..area
            };
            (content, Some(edge))
        };
        let list = Rect {
            y: area.y + 3,
            height: area.height.saturating_sub(5),
            ..area
        };
        Self {
            area,
            title: row(area, area.y),
            search: row(area, area.y + 1),
            rule: row(area, area.y + 2),
            list,
            footer_rule: row(area, area.bottom().saturating_sub(2)),
            footer: row(area, area.bottom().saturating_sub(1)),
            edge,
        }
    }
}

/// `percent` of `cells`, rounded down.
fn share(cells: u16, percent: u16) -> u16 {
    u16::try_from(u32::from(cells) * u32::from(percent) / 100).unwrap_or(cells)
}

impl Sidebar {
    /// The files pane laid out in `area`.
    fn of(area: Rect) -> Self {
        let header = Rect {
            height: area.height.min(1),
            ..area
        };
        let progress = Rect {
            y: header.bottom(),
            height: area.height.saturating_sub(header.height).min(2),
            ..area
        };
        let rule = Rect {
            y: progress.bottom(),
            height: area
                .height
                .saturating_sub(header.height + progress.height)
                .min(1),
            ..area
        };
        let tree = Rect {
            y: rule.bottom(),
            height: area.bottom().saturating_sub(rule.bottom()),
            ..area
        };
        Self {
            area,
            header,
            progress,
            rule,
            tree,
        }
    }
}

/// Row `y` of `area`, or an empty one when `area` has no such row.
fn row(area: Rect, y: u16) -> Rect {
    if y >= area.y && y < area.bottom() {
        Rect {
            y,
            height: 1,
            ..area
        }
    } else {
        Rect { height: 0, ..area }
    }
}

/// The widths of the two halves of a side-by-side diff `width` cells wide, each with its
/// gutter and sign; the right one takes an odd cell.
#[must_use]
pub fn halves(width: u16) -> (u16, u16) {
    let both = width.saturating_sub(FRAME + DIVIDER);
    let left = both / 2;
    (left, both - left)
}

/// The terminal's sizes, the same at every code size: every row is one cell tall and every
/// column one cell wide, and the bars keep their rows.
#[must_use]
pub fn sizes() -> Sizes {
    Sizes {
        row_height: 1.0,
        code_cell: 1.0,
        text_cell: 1.0,
        frame: f32::from(FRAME),
        divider: f32::from(DIVIDER),
        gutter: f32::from(GUTTER),
        sign: f32::from(SIGN),
        marker: f32::from(MARKER),
        // A cell of room after a buffer's line numbers.
        number_room: 1.0,
        half_slack: PAN_ROOM,
        // A hunk header keeps the last cell free, so the tool's rows have one cell less.
        note_spacing: 1.0,
        // A track click's page keeps one row of the page before.
        page_context: 1.0,
        chrome: Chrome::default(),
        panel_row_height: 1.0,
    }
}

/// The geometry of a diff pane `width` cells wide with lists `heights` tall, laid out at code
/// `code_size`: the diff and the buffer take the pane's width, and the halves of a
/// side-by-side diff take whole cells, the narrower on the left.
fn in_width(width: u16, code_size: f32, heights: Heights) -> Geometry {
    let (left, right) = halves(width);
    Geometry {
        laid_out: code_size,
        diff_width: f32::from(width),
        diff_list: f32::from(width),
        diff_half: f32::from(left.min(right)),
        buffer_list: f32::from(width),
        heights,
    }
}

/// The geometry the window opens with in `panes`, at code `code_size`, before any list showed.
#[must_use]
pub fn opening(panes: &Panes, code_size: f32) -> Geometry {
    in_width(panes.main.width, code_size, Heights::default())
}

/// The geometry of the frame laid out as `panes` for `model`: the diff pane's width, and the
/// heights of the lists that show; a list that does not show keeps the height it had in the
/// model's last geometry.
#[must_use]
pub fn geometry(panes: &Panes, model: &Model) -> Geometry {
    let mut heights = model.geometry().heights;
    let rows = Some(f32::from(panes.list.height));
    if model.mode == Mode::Buffer && model.buffer.is_some() {
        heights.buffer = rows;
    } else {
        heights.diff = rows;
    }
    if let Some(sidebar) = &panes.sidebar {
        heights.tree = Some(f32::from(sidebar.tree.height));
    }
    if let Some(picker) = &panes.picker {
        heights.picker = Some(f32::from(picker.list.height));
    }
    // The settings list's height goes with it, as the model asks only while it is open.
    heights.settings = panes
        .settings
        .map(|settings| f32::from(settings.list.height));
    in_width(panes.main.width, model.code_size, heights)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(sidebar: f32) -> PaneLayout {
        PaneLayout {
            sidebar,
            sidebar_visible: true,
        }
    }

    /// A 160×50 terminal: the files pane, its divider and the diff share the rows above the
    /// two bars, and the `/` bar takes a row above them while it shows.
    #[test]
    fn panes_share_the_screen() {
        let area = Rect::new(0, 0, 160, 50);
        let panes = Panes::lay_out(area, layout(32.0), true, false);
        let sidebar = panes.sidebar.unwrap();
        assert_eq!(sidebar.area, Rect::new(0, 0, 32, 48));
        assert_eq!(sidebar.tree, Rect::new(0, 4, 32, 44));
        assert_eq!(panes.divider, Some(Rect::new(32, 0, 1, 48)));
        assert_eq!(panes.main, Rect::new(33, 0, 127, 48));
        assert_eq!(panes.list, Rect::new(33, 1, 127, 47));
        assert_eq!(panes.status, Rect::new(0, 48, 160, 1));
        assert_eq!(panes.hints, Rect::new(0, 49, 160, 1));

        let finding = Panes::lay_out(area, layout(32.0), false, true);
        assert_eq!(finding.sidebar, None);
        assert_eq!(finding.find, Some(Rect::new(0, 47, 160, 1)));
        assert_eq!(finding.main, Rect::new(0, 0, 160, 47));
    }

    /// A screen too small for anything lays out empty panes instead of failing.
    #[test]
    fn tiny_screens_lay_out_empty_panes() {
        let panes = Panes::lay_out(Rect::new(0, 0, 3, 1), layout(32.0), true, true);
        assert_eq!(panes.list.height, 0);
        assert_eq!(panes.hints.height, 1);
    }

    /// Side by side needs 40 cells of code in each half: a wide diff has them, a narrow one
    /// shows unified.
    #[test]
    fn side_by_side_needs_room_for_both_halves() {
        let min = review_ui::model::present::MIN_HALF_CELLS;
        let scale = review_ui::frame::Scale::fixed(sizes());
        let metrics = review_ui::frame::Metrics {
            code_size: 17.0,
            density: review_ui::config::Density::Normal,
            panel: None,
        };
        let half_cells = |width| {
            in_width(width, 17.0, Heights::default())
                .grid(&scale, &metrics)
                .half_cells
        };
        assert!(half_cells(127) >= min);
        assert!(half_cells(96) >= min);
        assert!(half_cells(95) < min);
        assert_eq!(halves(127), (62, 63));
    }
}
