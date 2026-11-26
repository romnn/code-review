//! A changed image's comparison in a terminal that shows images: where it goes in the diff
//! pane, each version's image resized and encoded off the main loop, and which of them a frame
//! draws.
//!
//! A frame draws a version's image only when it was prepared for the raster on screen and the
//! cells the version has now, so an image prepared late never lands in another file's place,
//! in place of the other head, or at a size the pane no longer has; showing the changed pixels
//! prepares the head again, never the base.
//! An image takes at most [`MAX_PIXELS`] of the terminal's pixels, which bounds what is resized
//! and what each frame that first draws it sends.
//! One preparation runs at a time; what shows when it ends is prepared next.
//! An image's placeholders go as soon as anything draws over their cells, and the terminal
//! drops its data once another image replaces it, nothing shows it, or the app ends; see
//! [`Uploads`].

use std::hash::{BuildHasher as _, Hasher as _};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use image::DynamicImage;
use image::imageops::FilterType;
use ratatui::layout::{Rect, Size};
use ratatui_image::protocol::Protocol;
use ratatui_image::protocol::iterm2::Iterm2;
use ratatui_image::protocol::kitty::Kitty;
use review_ui::images::{Extent, Raster, fitted};
use review_ui::paint::images::{ComparisonPaint, VersionPaint};

use crate::app::Message;
use crate::graphics::{Graphics, Upload, Uploads};
use crate::measure::Panes;

/// The most of the list's rows the comparison takes, in percent.
const SHARE: u16 = 45;

/// The most pixels one version's image takes in the terminal: 4 bytes each, a third more in
/// base64, sent by the frame that first draws it.
pub const MAX_PIXELS: u32 = 512 * 1024;

/// The size of a terminal cell in pixels, as the terminal reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSize {
    /// The width.
    pub width: u16,
    /// The height.
    pub height: u16,
}

/// Where a comparison goes, at the top of the diff pane's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComparisonPanes {
    /// The whole comparison.
    pub area: Rect,
    /// Both versions' labels.
    pub labels: Rect,
    /// The base's room, on the left.
    pub base: Rect,
    /// The head's room, on the right.
    pub head: Rect,
    /// The line under them.
    pub status: Rect,
}

/// Carves a comparison out of the top of `panes`' list: a row of labels, both versions side
/// by side, and the line under them, always [`SHARE`] of the list, so the rows below stay
/// where they are while the comparison is pending and once it lands.
/// A list too small for it keeps its rows.
pub fn lay_out(panes: &mut Panes) {
    let list = panes.list;
    let most = u16::try_from(u32::from(list.height) * u32::from(SHARE) / 100).unwrap_or(0);
    let half = list.width.saturating_sub(1) / 2;
    // Labels, two rows of images and the status at least, and a few cells across.
    if most < 4 || half < 4 {
        return;
    }
    let area = Rect {
        height: most,
        ..list
    };
    let images = Rect {
        y: area.y + 1,
        height: most - 2,
        width: half,
        ..area
    };
    panes.comparison = Some(ComparisonPanes {
        area,
        labels: Rect { height: 1, ..area },
        base: images,
        head: Rect {
            x: images.right() + 1,
            width: area.width.saturating_sub(half + 1),
            ..images
        },
        status: Rect {
            y: images.bottom(),
            height: 1,
            ..area
        },
    });
    panes.list = Rect {
        y: area.bottom(),
        height: list.height - area.height,
        ..list
    };
}

/// Where `raster` draws in `room`, in cells `cell` pixels large: as large as fits, keeping its
/// aspect as far as whole cells allow, within [`MAX_PIXELS`], in the middle of its room.
#[must_use]
pub fn placement(raster: &Raster, room: Rect, cell: CellSize) -> Rect {
    let pixels = Extent {
        width: f32::from(room.width) * f32::from(cell.width),
        height: f32::from(room.height) * f32::from(cell.height),
    };
    let shown = fitted(raster.width(), raster.height(), pixels);
    // Smaller where it would take more pixels than the budget, which whole cells rounded down
    // keep it within.
    let budget = (f64::from(MAX_PIXELS) / (f64::from(shown.width) * f64::from(shown.height)))
        .sqrt()
        .min(1.0);
    let cells = |pixels: f32, cell: u16, most: u16| {
        let cells = f64::from(pixels) * budget / f64::from(cell);
        let cells = if budget < 1.0 {
            cells.floor()
        } else {
            cells.round()
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a part of the room's own cells, within u16"
        )]
        let cells = cells as u16;
        cells.clamp(1, most.max(1))
    };
    let mut width = cells(shown.width, cell.width, room.width);
    let mut height = cells(shown.height, cell.height, room.height);
    // Whole cells rounded up can still take more than the budget, which a cell less on the
    // longer side brings back within.
    let (wide, tall) = (u64::from(cell.width), u64::from(cell.height));
    while u64::from(width) * wide * u64::from(height) * tall > u64::from(MAX_PIXELS)
        && (width > 1 || height > 1)
    {
        if width > 1 && (u64::from(width) * wide >= u64::from(height) * tall || height == 1) {
            width -= 1;
        } else {
            height -= 1;
        }
    }
    Rect {
        x: room.x + (room.width - width) / 2,
        y: room.y + (room.height - height) / 2,
        width,
        height,
    }
}

/// What a version's image is prepared from and for: its raster, held so no other takes its
/// address, the cells it takes, their size in pixels, and how the terminal shows it.
#[derive(Clone)]
struct Want {
    raster: Raster,
    cells: Size,
    cell: CellSize,
    graphics: Graphics,
}

impl Want {
    /// Whether `self` and `other` make the same image.
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.raster, &other.raster)
            && self.cells == other.cells
            && self.cell == other.cell
            && self.graphics == other.graphics
    }
}

/// A version's image, ready to draw.
pub struct Placed {
    want: Want,
    protocol: Protocol,
    /// The kitty image the terminal holds once a frame draws it, which it must drop.
    upload: Option<Upload>,
}

/// One version's side of the terminal's images: what shows, and the image prepared for it.
#[derive(Default)]
struct Side {
    wanted: Option<Want>,
    placed: Option<Placed>,
}

impl Side {
    /// The image to draw: the one prepared, while it was prepared for what shows.
    fn drawn(&self) -> Option<&Protocol> {
        let placed = self.placed.as_ref()?;
        self.wanted
            .as_ref()
            .is_some_and(|wanted| wanted.same(&placed.want))
            .then_some(&placed.protocol)
    }

    /// What still needs preparing.
    fn missing(&self) -> Option<&Want> {
        self.wanted.as_ref().filter(|_| self.drawn().is_none())
    }

    /// Takes `landed` in while it was prepared for what shows, retiring the image before it;
    /// the terminal may hold it from the frame that first draws it on.
    fn land(&mut self, landed: Option<Placed>) {
        let Some(landed) = landed else {
            return;
        };
        if self
            .wanted
            .as_ref()
            .is_some_and(|wanted| wanted.same(&landed.want))
        {
            self.retire();
            if let Some(upload) = landed.upload {
                Uploads::hold(upload);
            }
            self.placed = Some(landed);
        }
    }

    /// Retires the image prepared, which the terminal drops with the next frame.
    fn retire(&mut self) {
        if let Some(upload) = self.placed.take().and_then(|placed| placed.upload) {
            Uploads::retire(upload.id);
        }
    }
}

/// Both versions' images, as a preparation hands them back.
pub struct Prepared {
    base: Option<Placed>,
    head: Option<Placed>,
}

/// What a frame draws of a comparison's images, prepared for its cells.
pub struct Drawn<'a> {
    /// The base's image.
    pub base: Option<&'a Protocol>,
    /// The head's image.
    pub head: Option<&'a Protocol>,
    /// The size of the cells they were prepared for.
    pub cell: CellSize,
}

/// The terminal's images: how it shows them, and each version's image prepared for it.
pub struct Images {
    graphics: Graphics,
    /// The cell size the terminal last reported, if it reports one.
    cell: Option<CellSize>,
    base: Side,
    head: Side,
    /// Whether a preparation runs.
    preparing: bool,
    /// The identity the next image takes.
    next_id: u32,
}

impl Images {
    /// No images, until the terminal is known to show them.
    #[must_use]
    pub fn new() -> Self {
        Self {
            graphics: Graphics::None,
            cell: None,
            base: Side::default(),
            head: Side::default(),
            preparing: false,
            // A random start, so images of other apps in the same terminal, which share its
            // identities, are not replaced.
            next_id: u32::try_from(std::hash::RandomState::new().build_hasher().finish() >> 40)
                .unwrap_or(1),
        }
    }

    /// Takes in how the terminal shows images, as it starts and whenever tmux says another
    /// client attached or anything else changed: the images prepared before are retired, since
    /// the terminal attached now may never have had them, and dropping an image a terminal
    /// does not hold does nothing.
    pub fn set_graphics(&mut self, graphics: Graphics) {
        self.graphics = graphics;
        self.base.retire();
        self.head.retire();
    }

    /// Takes in the terminal's cell size in pixels, as it reports it now.
    pub fn set_cell(&mut self, cell: Option<CellSize>) {
        self.cell = cell.filter(|cell| cell.width > 0 && cell.height > 0);
    }

    /// Whether images are being prepared.
    #[must_use]
    pub fn busy(&self) -> bool {
        self.preparing
    }

    /// The cell size images are drawn in, when the terminal shows images and reports one.
    #[must_use]
    pub fn cell(&self) -> Option<CellSize> {
        match self.graphics {
            Graphics::Kitty { .. } | Graphics::Iterm2 => self.cell,
            Graphics::None => None,
        }
    }

    /// Follows the comparison `shown` placed in `panes`, or none, as a frame lays out: the
    /// images are retired once no comparison shows, and the images still missing start
    /// preparing on a thread of its own, which hands them to `sender`, unless a preparation
    /// runs.
    pub fn follow(
        &mut self,
        shown: Option<&ComparisonPaint>,
        panes: &Panes,
        sender: &Sender<Message>,
    ) {
        let (graphics, Some(cell)) = (self.graphics, self.cell) else {
            self.base.wanted = None;
            self.head.wanted = None;
            return;
        };
        let placed = shown.zip(panes.comparison);
        let want = |version: &VersionPaint, room: Rect| {
            let raster = version.raster.clone()?;
            let cells = placement(&raster, room, cell).as_size();
            Some(Want {
                raster,
                cells,
                cell,
                graphics,
            })
        };
        self.base.wanted = placed.and_then(|(shown, panes)| want(&shown.base, panes.base));
        self.head.wanted = placed.and_then(|(shown, panes)| want(&shown.head, panes.head));
        // While a comparison shows, pending included, an image goes only with the one that
        // replaces it, in the frame that sends that one.
        if placed.is_none() {
            self.base.retire();
            self.head.retire();
        }
        let base = self.base.missing().cloned();
        let head = self.head.missing().cloned();
        if self.preparing || (base.is_none() && head.is_none()) {
            return;
        }
        self.preparing = true;
        let order = Order {
            base: base.map(|want| Part {
                want,
                id: self.take_id(),
            }),
            head: head.map(|want| Part {
                want,
                id: self.take_id(),
            }),
        };
        let sender = sender.clone();
        std::thread::spawn(move || {
            // The main loop is gone once the app quits; nothing waits for this.
            sender
                .send(Message::Prepared(Box::new(order.prepare())))
                .ok();
        });
    }

    /// Takes in a preparation that ended: each image prepared for what shows now replaces the
    /// one before it.
    pub fn prepared(&mut self, prepared: Prepared) {
        self.preparing = false;
        let Prepared { base, head } = prepared;
        self.base.land(base);
        self.head.land(head);
    }

    /// The images a frame draws: each version's prepared for what shows, once it is.
    #[must_use]
    pub fn drawn(&self) -> Option<Drawn<'_>> {
        let (base, head) = (self.base.drawn(), self.head.drawn());
        if base.is_none() && head.is_none() {
            return None;
        }
        Some(Drawn {
            base,
            head,
            cell: self.cell?,
        })
    }

    /// A new image identity: never 0, and never 0 in the 24 bits a placeholder's color holds.
    fn take_id(&mut self) -> u32 {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if id & 0x00ff_ffff != 0 {
                return id;
            }
        }
    }
}

/// One version's image to prepare, and the identity a kitty image takes.
struct Part {
    want: Want,
    id: u32,
}

/// The images to prepare.
struct Order {
    base: Option<Part>,
    head: Option<Part>,
}

impl Order {
    /// Resizes each raster to the pixels of its cells, within [`MAX_PIXELS`], and encodes it
    /// for the terminal; runs on a thread of its own.
    fn prepare(self) -> Prepared {
        let Self { base, head } = self;
        Prepared {
            base: base.and_then(place),
            head: head.and_then(place),
        }
    }
}

/// `part`'s image, resized to the pixels of its cells and encoded as its terminal shows it.
fn place(part: Part) -> Option<Placed> {
    let Part { want, id } = part;
    let width = u32::from(want.cells.width) * u32::from(want.cell.width);
    let height = u32::from(want.cells.height) * u32::from(want.cell.height);
    // Pixels grown larger keep their edges, so a small icon's changes stay sharp.
    let filter = if width > want.raster.width() {
        FilterType::Nearest
    } else {
        FilterType::Triangle
    };
    let resized = DynamicImage::ImageRgba8(image::imageops::resize(
        &*want.raster,
        width,
        height,
        filter,
    ));
    let (protocol, upload) = match want.graphics {
        Graphics::Kitty { tmux } => (
            Protocol::Kitty(Kitty::new(resized, want.cells, id, tmux, false).ok()?),
            Some(Upload { id, tmux }),
        ),
        Graphics::Iterm2 => (
            Protocol::ITerm2(Iterm2::new(resized, want.cells, false).ok()?),
            None,
        ),
        Graphics::None => return None,
    };
    Some(Placed {
        want,
        protocol,
        upload,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    use image::RgbaImage;
    use review_ui::color::Hsla;
    use review_ui::model::PaneLayout;
    use review_ui::paint::Piece;

    use super::*;

    /// Cells of 10 × 20 pixels.
    const CELL: CellSize = CellSize {
        width: 10,
        height: 20,
    };

    /// A 640 × 400 raster.
    fn raster() -> Raster {
        Arc::new(RgbaImage::new(640, 400))
    }

    /// A comparison of `base` and `head`, the head tinted when `tinted`.
    fn shown(base: &Raster, head: &Raster, tinted: bool) -> ComparisonPaint {
        let version = |name: &str, raster: &Raster| VersionPaint {
            name: Piece::new(name, Hsla::default()),
            detail: Piece::new("PNG · 640 × 400", Hsla::default()),
            raster: Some(Arc::clone(raster)),
            note: None,
        };
        ComparisonPaint {
            base: version("base", base),
            head: version("head", head),
            tinted,
            status: Vec::new(),
        }
    }

    /// A `width` × 50 terminal's panes without the files, with a comparison laid out.
    fn panes(width: u16) -> Panes {
        let layout = PaneLayout {
            sidebar: review_ui::panes::SidebarWidth::Fit,
            comparison: None,
            sidebar_visible: false,
        };
        let mut panes = Panes::lay_out(Rect::new(0, 0, width, 50), layout, 0, false, false);
        lay_out(&mut panes);
        panes
    }

    /// A comparison still being made: labels and nothing to draw.
    fn pending() -> ComparisonPaint {
        let version = |name: &str| VersionPaint {
            name: Piece::new(name, Hsla::default()),
            detail: Piece::new("", Hsla::default()),
            raster: None,
            note: None,
        };
        ComparisonPaint {
            base: version("base"),
            head: version("head"),
            tinted: false,
            status: Vec::new(),
        }
    }

    /// What the preparation started by the last [`Images::follow`] handed back.
    fn prepared(receiver: &Receiver<Message>) -> Prepared {
        match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(Message::Prepared(prepared)) => *prepared,
            _ => panic!("no images were prepared"),
        }
    }

    /// Which versions a frame draws an image of.
    fn drawn(images: &Images) -> [bool; 2] {
        images.drawn().map_or([false, false], |drawn| {
            [drawn.base.is_some(), drawn.head.is_some()]
        })
    }

    /// The comparison takes its share of the top of the list, whatever its images: labels,
    /// the versions side by side, and the line under them; 640 × 400 pixels fit 61 × 19 cells
    /// of a half's 80 × 19, in the middle of it.
    #[test]
    fn the_comparison_takes_its_share() {
        let raster = raster();
        let panes = panes(161);
        let placed = panes.comparison.expect("the comparison fits");
        assert_eq!(placed.area, Rect::new(0, 1, 161, 21));
        assert_eq!(placed.base, Rect::new(0, 2, 80, 19));
        assert_eq!(placed.head, Rect::new(81, 2, 80, 19));
        assert_eq!(placed.status, Rect::new(0, 21, 161, 1));
        assert_eq!(panes.list, Rect::new(0, 22, 161, 26));
        assert_eq!(
            placement(&raster, placed.base, CELL),
            Rect::new(9, 2, 61, 19)
        );
    }

    /// In a terminal of large cells, an image that would fill its room takes no more of the
    /// terminal's pixels than the budget, which bounds what is resized and sent.
    #[test]
    fn an_image_stays_within_the_pixel_budget() {
        let large = CellSize {
            width: 20,
            height: 40,
        };
        let wide: Raster = Arc::new(RgbaImage::new(2048, 1024));
        let shown = placement(&wide, Rect::new(0, 0, 150, 60), large);
        let pixels = u32::from(shown.width)
            * u32::from(large.width)
            * u32::from(shown.height)
            * u32::from(large.height);
        assert!(pixels <= MAX_PIXELS, "{shown:?} takes {pixels} pixels");
        // Still most of the budget, keeping the image's aspect of two to one.
        assert!(pixels > MAX_PIXELS / 2, "{shown:?} takes {pixels} pixels");
        let aspect = (shown.width * large.width) / (shown.height * large.height);
        assert_eq!(aspect, 2);
    }

    /// Whole cells rounded up never take the image past the budget: 727 × 720 pixels fit
    /// within it, but rounded to cells of 12 × 24 pixels they would take 732 × 720.
    #[test]
    fn rounding_to_cells_stays_within_the_budget() {
        let cell = CellSize {
            width: 12,
            height: 24,
        };
        let raster: Raster = Arc::new(RgbaImage::new(727, 720));
        let shown = placement(&raster, Rect::new(0, 0, 61, 30), cell);
        let pixels = u32::from(shown.width)
            * u32::from(cell.width)
            * u32::from(shown.height)
            * u32::from(cell.height);
        assert!(pixels <= MAX_PIXELS, "{shown:?} takes {pixels} pixels");
        assert_eq!((shown.width, shown.height), (60, 30));
    }

    /// A frame draws only images prepared for what shows: showing the changed pixels keeps
    /// the base's image and prepares the head's alone, a head prepared for a choice no longer
    /// on screen never draws, and the images go once nothing shows them or another client
    /// attaches, which gets them prepared afresh.
    #[test]
    fn a_frame_draws_only_images_prepared_for_what_shows() {
        let _registry = crate::graphics::alone();
        let (sender, receiver) = mpsc::channel();
        let mut images = Images::new();
        images.set_graphics(Graphics::Kitty { tmux: false });
        images.set_cell(Some(CELL));
        let (base, head, tint) = (raster(), raster(), raster());
        let plain = shown(&base, &head, false);
        let tinted = shown(&base, &tint, true);
        let panes = panes(161);

        images.follow(Some(&plain), &panes, &sender);
        assert_eq!(drawn(&images), [false, false], "nothing is prepared yet");
        images.prepared(prepared(&receiver));
        assert_eq!(drawn(&images), [true, true]);

        // The tint shows: the base keeps its image while the head's is prepared alone.
        images.follow(Some(&tinted), &panes, &sender);
        assert_eq!(drawn(&images), [true, false]);
        let late = prepared(&receiver);
        assert!(late.base.is_none() && late.head.is_some());
        // The tint is hidden again before it lands: the plain head draws, the late one never.
        images.follow(Some(&plain), &panes, &sender);
        assert_eq!(drawn(&images), [true, true]);
        images.prepared(late);
        images.follow(Some(&plain), &panes, &sender);
        assert_eq!(drawn(&images), [true, true]);
        assert_eq!(Uploads::drops(false), "", "an image went while it showed");

        // Another image's comparison is pending: nothing draws, and the images stay until
        // theirs replace them.
        images.follow(Some(&pending()), &panes, &sender);
        assert_eq!(drawn(&images), [false, false]);
        assert_eq!(Uploads::drops(false), "");

        // Another file shows: nothing draws, and both images go.
        images.follow(None, &panes, &sender);
        assert_eq!(drawn(&images), [false, false]);
        assert_eq!(Uploads::drops(false).matches("a=d").count(), 2);

        // Another client attaches: its terminal may never have had the images, so they are
        // prepared afresh, and the old ones dropped, in case the terminal still holds them.
        images.follow(Some(&plain), &panes, &sender);
        images.prepared(prepared(&receiver));
        images.set_graphics(Graphics::Kitty { tmux: true });
        assert_eq!(Uploads::drops(false).matches("a=d").count(), 2);
        images.follow(Some(&plain), &panes, &sender);
        assert_eq!(drawn(&images), [false, false]);
        let fresh = prepared(&receiver);
        assert!(fresh.base.is_some() && fresh.head.is_some());
    }
}
