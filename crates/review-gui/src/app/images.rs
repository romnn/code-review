//! A changed image's comparison above its diff: each version beside the other under its label,
//! and the line under both, as [`review_ui::paint::images`] says.
//!
//! gpui draws a raster as a [`RenderImage`] of BGRA pixels: each of the model's rasters is
//! converted on a thread of its own the first time it would draw, its half staying empty until
//! the conversion lands, and installed once.
//! A converted raster the model let go of is dropped at once, from the sprite atlas too, while a
//! conversion still running stays until it lands, so a raster the model holds again meanwhile
//! never converts twice.
//! An image draws only for a raster of the comparison shown, never under another file.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{Context, Div, ObjectFit, RenderImage, Window, div, img, px};
use gpui_component::{h_flex, v_flex};
use review_ui::images::Raster;
use review_ui::model::Model;
use review_ui::paint::Palette;
use review_ui::paint::images::{self, VersionPaint};

use super::render::GRID;
use super::{ReviewApp, ink};
use crate::color::to_gpui;

/// The window's height the comparison takes.
const SHARE: f32 = 0.45;

/// A raster as gpui draws it.
struct Rendered {
    /// The model's raster, held so no other raster takes its address while it is cached.
    raster: Raster,
    /// Its image, once converted.
    image: Option<Arc<RenderImage>>,
}

/// What [`Images::find`] found for a raster.
enum Found {
    /// Its image, ready to draw.
    Ready(Arc<RenderImage>),
    /// Its conversion runs.
    Converting,
    /// Nothing yet: its conversion is to start.
    Start,
}

/// The rasters converted for gpui.
#[derive(Default)]
pub(super) struct Images(Vec<Rendered>);

impl Images {
    /// Drops the converted rasters the model no longer holds, from the sprite atlas too.
    pub(super) fn retain(&mut self, model: &Model, window: &mut Window) {
        let held: Vec<&Raster> = model
            .comparison()
            .map(|compared| compared.comparison.rasters().collect())
            .unwrap_or_default();
        for image in self.release(&held) {
            // A failure leaves the image in the atlas, which only costs its memory.
            window.drop_image(image).ok();
        }
    }

    /// Lets go of the converted rasters not `held`, keeping the conversions still running;
    /// returns their images, to drop from the atlas.
    fn release(&mut self, held: &[&Raster]) -> Vec<Arc<RenderImage>> {
        let mut released = Vec::new();
        self.0.retain(|rendered| {
            let Some(image) = &rendered.image else {
                return true;
            };
            let keep = held
                .iter()
                .any(|raster| Arc::ptr_eq(raster, &rendered.raster));
            if !keep {
                released.push(Arc::clone(image));
            }
            keep
        });
        released
    }

    /// Whether a conversion runs, which QA mode waits for.
    pub(super) fn converting(&self) -> bool {
        self.0.iter().any(|rendered| rendered.image.is_none())
    }

    /// What there is of `raster`'s image, noting the conversion that [`Found::Start`] asks for.
    fn find(&mut self, raster: &Raster) -> Found {
        let found = self
            .0
            .iter()
            .find(|rendered| Arc::ptr_eq(&rendered.raster, raster));
        match found.map(|rendered| rendered.image.clone()) {
            Some(Some(image)) => Found::Ready(image),
            Some(None) => Found::Converting,
            None => {
                self.0.push(Rendered {
                    raster: Arc::clone(raster),
                    image: None,
                });
                Found::Start
            }
        }
    }

    /// Installs `image`, converted from `raster`, unless it has one; returns whether it did.
    fn install(&mut self, raster: &Raster, image: Arc<RenderImage>) -> bool {
        let converting = self
            .0
            .iter_mut()
            .find(|rendered| Arc::ptr_eq(&rendered.raster, raster) && rendered.image.is_none());
        let Some(rendered) = converting else {
            return false;
        };
        rendered.image = Some(image);
        true
    }

    /// The image of `raster`, once converted; the first time, the conversion starts on a
    /// thread of its own.
    fn image(&mut self, raster: &Raster, cx: &mut Context<ReviewApp>) -> Option<Arc<RenderImage>> {
        match self.find(raster) {
            Found::Ready(image) => return Some(image),
            Found::Converting => return None,
            Found::Start => {}
        }
        let (sender, receiver) = futures_channel::oneshot::channel();
        std::thread::spawn({
            let raster = Arc::clone(raster);
            move || {
                // The window is gone once the app quits; nothing waits for this.
                sender.send(bgra(&raster)).ok();
            }
        });
        let raster = Arc::clone(raster);
        cx.spawn(async move |this, cx| {
            let Ok(image) = receiver.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if this.images.install(&raster, image) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        None
    }
}

/// `raster` as gpui draws it, with its red and blue swapped.
fn bgra(raster: &Raster) -> Arc<RenderImage> {
    let mut pixels = (**raster).clone();
    let (rgba, _) = pixels.as_chunks_mut::<4>();
    for [red, _, blue, _] in rgba {
        std::mem::swap(red, blue);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]))
}

impl ReviewApp {
    /// The comparison above the diff, while the selected file's versions are images or may be:
    /// the labels, both versions fitted into halves of the pane, and the line under them,
    /// always [`SHARE`] of the window's height, so the rows below never move when it lands.
    pub(super) fn render_comparison(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let colors = self.palette();
        let shown = images::comparison(self, &colors)?;
        let bar = self.sizes().bar();
        let gap = GRID * 3.0;
        let height = (f32::from(window.viewport_size().height) * SHARE - bar * 2.0).max(bar);
        let base = self.comparison_half(&shown.base, &colors, cx);
        let head = self.comparison_half(&shown.head, &colors, cx);
        let status = h_flex()
            .h(px(bar))
            .whitespace_nowrap()
            .children(shown.status.into_iter().map(ink::piece));
        let label = |version: &VersionPaint| {
            h_flex()
                .flex_1()
                .min_w_0()
                .gap(px(GRID * 2.0))
                .whitespace_nowrap()
                .child(ink::piece(version.name.clone()))
                .child(ink::piece(version.detail.clone()).min_w_0().truncate())
        };
        Some(
            v_flex()
                .flex_none()
                .w_full()
                .px(px(gap))
                .bg(to_gpui(colors.surface))
                .border_b_1()
                .border_color(to_gpui(colors.hairline))
                .text_size(px(self.sizes().ui_size()))
                .child(
                    h_flex()
                        .h(px(bar))
                        .gap(px(gap))
                        .child(label(&shown.base))
                        .child(label(&shown.head)),
                )
                .child(h_flex().h(px(height)).gap(px(gap)).child(base).child(head))
                .child(status),
        )
    }

    /// One version's half: its raster fitted into it, or what shows in its place.
    fn comparison_half(
        &mut self,
        version: &VersionPaint,
        colors: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let half = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(to_gpui(colors.background));
        match (&version.raster, &version.note) {
            // Empty until the raster's conversion lands.
            (Some(raster), _) => half.children(
                self.images
                    .image(raster, cx)
                    .map(|image| img(image).object_fit(ObjectFit::Contain).size_full()),
            ),
            (None, Some(note)) => half.child(ink::piece(note.clone())),
            (None, None) => half,
        }
    }
}

#[cfg(test)]
mod tests {
    use image::RgbaImage;

    use super::*;

    /// A raster the model lets go of while it converts and then holds again converts once, its
    /// image is installed once, and it goes, from the atlas too, as soon as the model lets go
    /// of it, while another raster still converts.
    #[test]
    fn a_conversion_is_kept_until_it_lands_and_installed_once() {
        let raster: Raster = Arc::new(RgbaImage::new(2, 2));
        let mut images = Images::default();
        assert!(matches!(images.find(&raster), Found::Start));

        assert_eq!(images.release(&[]).len(), 0);
        assert!(
            matches!(images.find(&raster), Found::Converting),
            "a second conversion started"
        );
        let first = bgra(&raster);
        assert!(images.install(&raster, Arc::clone(&first)));
        assert!(
            !images.install(&raster, bgra(&raster)),
            "an installed image was replaced"
        );
        assert!(matches!(images.find(&raster), Found::Ready(image) if Arc::ptr_eq(&image, &first)));

        let next: Raster = Arc::new(RgbaImage::new(3, 3));
        assert!(matches!(images.find(&next), Found::Start));
        let released = images.release(&[&next]);
        assert_eq!(released.len(), 1);
        assert!(released.iter().all(|image| Arc::ptr_eq(image, &first)));
        assert!(matches!(images.find(&next), Found::Converting));
    }
}
