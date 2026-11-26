//! The comparison of a changed image's two versions: each decoded within fixed bounds, and
//! where both have one size, the head with the pixels that changed tinted.
//!
//! It takes a pair of encoded versions rather than a file, so anything that renders to pixels
//! can feed the same comparison.
//! Every bound is checked before the memory it guards is taken: the blob's size before it is
//! read, and the image's pixels and decoded size before it is decoded.
//! The rasters a comparison keeps are at most [`DISPLAY_EDGE`] pixels on their long side, which
//! any front end can draw as they are; the changed pixels are counted and tinted at full size,
//! before fitting.

use std::io::Cursor;
use std::sync::Arc;

use image::codecs::gif::GifDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, RgbaImage};

/// The largest blob the comparison reads, in bytes: the engine's ceiling for diffing a blob.
pub const MAX_BYTES: u64 = review_core::git::MAX_TEXT_BLOB_BYTES;

/// The most pixels one version may have to be decoded: 4096 × 4096.
pub const MAX_PIXELS: u64 = 4096 * 4096;

/// The most memory one version's decoder may take, in bytes, which a 16-bit image of
/// [`MAX_PIXELS`] needs.
pub const MAX_DECODED: u64 = 256 * 1024 * 1024;

/// The longest side of a raster the comparison keeps, in pixels.
pub const DISPLAY_EDGE: u32 = 2048;

/// The tint over a changed pixel, as RGBA: magenta, which few images use, so it stands out.
const TINT: [u8; 4] = [255, 0, 255, 255];

/// How much of the tint a changed pixel takes, out of 255, and the least alpha it keeps.
const TINT_WEIGHT: u8 = 160;

/// A raster to draw, RGBA with 8 bits a channel and straight alpha.
pub type Raster = Arc<RgbaImage>;

/// One version as the job read it from git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoded {
    /// The file has no such version: it was added, or deleted.
    Absent,
    /// The blob's content.
    Bytes(Vec<u8>),
    /// A blob larger than [`MAX_BYTES`], with its size, left unread.
    TooLarge(u64),
    /// The blob could not be read.
    Unreadable,
}

/// One version of the file as the comparison shows it.
#[derive(Debug, Clone, PartialEq)]
pub enum Version {
    /// The file has no such version: it was added, or deleted.
    Absent,
    /// The version decoded.
    Image(Picture),
    /// A blob larger than [`MAX_BYTES`], with its size in bytes, which was not read.
    TooLarge(u64),
    /// An image with more pixels than [`MAX_PIXELS`], or one that would take more than
    /// [`MAX_DECODED`] to decode, which was not decoded.
    TooManyPixels(Size),
    /// An image that could not be decoded, in the format named when it was recognized, or a
    /// blob that could not be read.
    Unreadable(Option<String>),
    /// Content in no image format the comparison knows.
    NotImage,
}

/// An image's format and size in pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Size {
    /// The format, such as `PNG`.
    pub format: String,
    /// The width: in pixels, or a PDF page's in points.
    pub width: u32,
    /// The height, in the width's unit.
    pub height: u32,
}

/// A decoded version.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    /// Its format and full size.
    pub size: Size,
    /// The image, fitted within [`DISPLAY_EDGE`]: the first frame of an animation.
    pub raster: Raster,
    /// Whether it was decoded as an animation, of which only the first frame shows and is
    /// compared: every GIF, an APNG and an animated WebP.
    pub first_frame: bool,
}

/// What changed between the versions' pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum Changes {
    /// Both versions decoded at one size, as they show: an animation's first frame, and a still
    /// image turned as its orientation says.
    Pixels {
        /// The head's raster with every changed pixel tinted, a pixel that became transparent
        /// included, fitted as the head's is.
        tinted: Raster,
        /// How many pixels differ.
        changed: u64,
        /// How many pixels each version has.
        total: u64,
    },
    /// Both versions decoded, at different sizes, so no pixels are compared.
    Sizes,
    /// Fewer than two versions decoded.
    None,
}

/// A changed image's two versions, and what changed between their pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// The base's version.
    pub base: Version,
    /// The head's version.
    pub head: Version,
    /// What changed between them.
    pub changes: Changes,
}

impl Comparison {
    /// Compares `base` with `head`, decoding each within the bounds; runs in the background.
    #[must_use]
    pub fn of(base: Encoded, head: Encoded) -> Self {
        Self::between(decode(base), decode(head))
    }

    /// Compares two versions already decoded, or each the version it shows instead, as a
    /// rendered PDF page is; runs in the background.
    pub(crate) fn between(base: Result<Decoded, Version>, head: Result<Decoded, Version>) -> Self {
        let changes = match (&base, &head) {
            (Ok(base), Ok(head))
                if base.image.dimensions() == head.image.dimensions()
                    && base.extent.map(f64::to_bits) == head.extent.map(f64::to_bits) =>
            {
                let (tinted, changed) = tint(&base.image, &head.image);
                let (width, height) = head.image.dimensions();
                Changes::Pixels {
                    tinted: Arc::new(fit(tinted)),
                    changed,
                    total: u64::from(width) * u64::from(height),
                }
            }
            (Ok(_), Ok(_)) => Changes::Sizes,
            _ => Changes::None,
        };
        Self {
            base: base.map_or_else(|version| version, Decoded::shown),
            head: head.map_or_else(|version| version, Decoded::shown),
            changes,
        }
    }

    /// Every raster the comparison holds: each decoded version's and the tinted head.
    pub fn rasters(&self) -> impl Iterator<Item = &Raster> {
        fn picture(version: &Version) -> Option<&Raster> {
            match version {
                Version::Image(picture) => Some(&picture.raster),
                _ => None,
            }
        }
        let tinted = match &self.changes {
            Changes::Pixels { tinted, .. } => Some(tinted),
            Changes::Sizes | Changes::None => None,
        };
        [picture(&self.base), picture(&self.head), tinted]
            .into_iter()
            .flatten()
    }

    /// Whether either version is an image, decoded or not, so the comparison is worth showing;
    /// otherwise the file shows as any other binary file does.
    #[must_use]
    pub fn has_image(&self) -> bool {
        [&self.base, &self.head].into_iter().any(|version| {
            matches!(
                version,
                Version::Image(_) | Version::TooManyPixels(_) | Version::Unreadable(Some(_))
            )
        })
    }
}

/// A version decoded at its full size, before it is fitted.
pub(crate) struct Decoded {
    size: Size,
    image: RgbaImage,
    first_frame: bool,
    /// The version's size exactly, which `size` rounds for its label: pixels for an image,
    /// points for a PDF page.
    extent: [f64; 2],
}

impl Decoded {
    /// `image`, a still raster in the format `format` names, within the decoding bounds, of
    /// a document whose size, such as a page's in points, is `[width, height]`.
    pub(crate) fn sized(format: String, image: RgbaImage, [width, height]: [f32; 2]) -> Self {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a document's size, rounded for its label, positive and far below u32::MAX"
        )]
        let rounded = |side: f32| side.round().max(0.0) as u32;
        Self {
            size: Size {
                format,
                width: rounded(width),
                height: rounded(height),
            },
            image,
            first_frame: false,
            extent: [f64::from(width), f64::from(height)],
        }
    }

    /// The version as the comparison shows it, fitted within [`DISPLAY_EDGE`].
    fn shown(self) -> Version {
        Version::Image(Picture {
            size: self.size,
            raster: Arc::new(fit(self.image)),
            first_frame: self.first_frame,
        })
    }
}

/// Decodes `encoded` within the bounds, or says why it shows no image.
///
/// Each format's decoder takes the bounds as it is made, and the image's header says its size
/// before any pixel is decoded.
/// An animation shows its first frame, an APNG's first animation frame rather than its default
/// image, and a still image as its orientation says it shows.
fn decode(encoded: Encoded) -> Result<Decoded, Version> {
    let bytes = match encoded {
        Encoded::Absent => return Err(Version::Absent),
        Encoded::TooLarge(bytes) => return Err(Version::TooLarge(bytes)),
        Encoded::Unreadable => return Err(Version::Unreadable(None)),
        Encoded::Bytes(bytes) => bytes,
    };
    let mut reader = image::ImageReader::new(Cursor::new(bytes.as_slice()))
        .with_guessed_format()
        .map_err(|_| Version::Unreadable(None))?;
    let Some(format) = reader.format() else {
        return Err(Version::NotImage);
    };
    let name = format_name(format);
    let unreadable = |_| Version::Unreadable(Some(name.clone()));
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODED);
    let cursor = || Cursor::new(bytes.as_slice());
    // A GIF is decoded as an animation whether it has one frame or more.
    let (image, first_frame) = match format {
        ImageFormat::Png => {
            let decoder = PngDecoder::with_limits(cursor(), limits).map_err(unreadable)?;
            bounded(&decoder, &name)?;
            if decoder.is_apng().map_err(unreadable)? {
                first_frame(decoder.apng().map_err(unreadable)?).map(|image| (image, true))
            } else {
                still(decoder).map(|image| (image, false))
            }
        }
        ImageFormat::Gif => {
            let mut decoder = GifDecoder::new(cursor()).map_err(unreadable)?;
            decoder.set_limits(limits).map_err(unreadable)?;
            bounded(&decoder, &name)?;
            first_frame(decoder).map(|image| (image, true))
        }
        ImageFormat::WebP => {
            let mut decoder = WebPDecoder::new(cursor()).map_err(unreadable)?;
            decoder.set_limits(limits).map_err(unreadable)?;
            bounded(&decoder, &name)?;
            if decoder.has_animation() {
                first_frame(decoder).map(|image| (image, true))
            } else {
                still(decoder).map(|image| (image, false))
            }
        }
        _ => {
            reader.limits(limits);
            let decoder = reader.into_decoder().map_err(unreadable)?;
            bounded(&decoder, &name)?;
            still(decoder).map(|image| (image, false))
        }
    }
    .map_err(unreadable)?;
    let (width, height) = image.dimensions();
    Ok(Decoded {
        size: Size {
            format: name.clone(),
            width,
            height,
        },
        image,
        first_frame,
        extent: [f64::from(width), f64::from(height)],
    })
}

/// Refuses the image `decoder` reads, in the format `name`, when it has more pixels than
/// [`MAX_PIXELS`] or would take more than [`MAX_DECODED`] to decode.
fn bounded(decoder: &impl ImageDecoder, name: &str) -> Result<(), Version> {
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > MAX_PIXELS || decoder.total_bytes() > MAX_DECODED {
        return Err(Version::TooManyPixels(Size {
            format: name.to_owned(),
            width,
            height,
        }));
    }
    Ok(())
}

/// The still image `decoder` reads, turned as its orientation says it shows.
fn still(mut decoder: impl ImageDecoder) -> image::ImageResult<RgbaImage> {
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image.into_rgba8())
}

/// The first frame of the animation `decoder` reads, the only one decoded.
fn first_frame<'a>(decoder: impl AnimationDecoder<'a>) -> image::ImageResult<RgbaImage> {
    let frame = decoder.into_frames().next().ok_or_else(|| {
        image::ImageError::Parameter(image::error::ParameterError::from_kind(
            image::error::ParameterErrorKind::NoMoreData,
        ))
    })??;
    Ok(frame.into_buffer())
}

/// The name a label gives `format`, such as `PNG`.
fn format_name(format: ImageFormat) -> String {
    match format {
        ImageFormat::Jpeg => "JPEG".to_owned(),
        ImageFormat::WebP => "WebP".to_owned(),
        other => other.extensions_str().first().map_or_else(
            || format!("{other:?}"),
            |extension| extension.to_uppercase(),
        ),
    }
}

/// `head` with every pixel that differs from `base`'s tinted, and how many differ; both are
/// the same size.
///
/// A changed pixel takes [`TINT_WEIGHT`] of the tint and at least the tint's weight in alpha,
/// so a pixel that became transparent shows the tint too.
fn tint(base: &RgbaImage, head: &RgbaImage) -> (RgbaImage, u64) {
    let mut tinted = head.clone();
    let mut changed = 0;
    for (shown, was) in tinted.pixels_mut().zip(base.pixels()) {
        if *shown == *was {
            continue;
        }
        changed += 1;
        let [red, green, blue, alpha] = shown.0;
        let [tint_red, tint_green, tint_blue, _] = TINT;
        let weight = u16::from(TINT_WEIGHT);
        let blend = |channel: u8, tint: u8| {
            let mixed = (u16::from(channel) * (255 - weight) + u16::from(tint) * weight) / 255;
            u8::try_from(mixed).unwrap_or(u8::MAX)
        };
        shown.0 = [
            blend(red, tint_red),
            blend(green, tint_green),
            blend(blue, tint_blue),
            alpha.max(TINT_WEIGHT),
        ];
    }
    (tinted, changed)
}

/// `image` scaled down to at most [`DISPLAY_EDGE`] pixels on its long side, or as it is.
fn fit(image: RgbaImage) -> RgbaImage {
    let (width, height) = image.dimensions();
    let long = width.max(height);
    if long <= DISPLAY_EDGE {
        return image;
    }
    let scaled = |side: u32| {
        let side = u64::from(side) * u64::from(DISPLAY_EDGE) / u64::from(long);
        u32::try_from(side).unwrap_or(DISPLAY_EDGE).max(1)
    };
    image::imageops::thumbnail(&image, scaled(width), scaled(height))
}

/// A width and height in a front end's units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extent {
    /// The width.
    pub width: f32,
    /// The height.
    pub height: f32,
}

/// The extent a raster `width` × `height` pixels large takes scaled to fill `room` without
/// leaving it, keeping its aspect.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "pixel sizes within the decoding bounds, far below 2^24, where f32 is exact"
)]
pub fn fitted(width: u32, height: u32, room: Extent) -> Extent {
    if width == 0 || height == 0 {
        return Extent {
            width: 0.0,
            height: 0.0,
        };
    }
    let (width, height) = (width as f32, height as f32);
    let scale = (room.width / width).min(room.height / height).max(0.0);
    Extent {
        width: width * scale,
        height: height * scale,
    }
}

#[cfg(test)]
mod tests {
    use image::Rgba;

    use super::*;

    /// `image` encoded as `format`.
    fn bytes(image: &RgbaImage, format: ImageFormat) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut bytes, format)
            .unwrap();
        bytes.into_inner()
    }

    /// `image` encoded as `format`, as the job reads a version.
    fn encoded(image: &RgbaImage, format: ImageFormat) -> Encoded {
        Encoded::Bytes(bytes(image, format))
    }

    /// A `width` × `height` image of one opaque color.
    fn filled(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba(color))
    }

    /// The decoded version's picture, or a panic naming what came instead.
    fn picture(version: &Version) -> &Picture {
        match version {
            Version::Image(picture) => picture,
            other => panic!("not an image: {other:?}"),
        }
    }

    /// Versions of one size count the pixels that changed, and the tinted head marks exactly
    /// those, a pixel that became transparent included.
    #[test]
    fn same_sizes_count_and_tint_the_changed_pixels() {
        let base = filled(4, 3, [10, 20, 30, 255]);
        let mut head = base.clone();
        head.put_pixel(1, 1, Rgba([200, 20, 30, 255]));
        head.put_pixel(3, 2, Rgba([0, 0, 0, 0]));
        let comparison = Comparison::of(
            encoded(&base, ImageFormat::Png),
            encoded(&head, ImageFormat::Png),
        );

        let Changes::Pixels {
            tinted,
            changed,
            total,
        } = &comparison.changes
        else {
            panic!("no pixels compared: {:?}", comparison.changes);
        };
        assert_eq!((*changed, *total), (2, 12));
        // Unchanged pixels keep the head's color; changed ones lean to the tint, and the one
        // that became transparent shows it.
        assert_eq!(tinted.get_pixel(0, 0), head.get_pixel(0, 0));
        assert_ne!(tinted.get_pixel(1, 1), head.get_pixel(1, 1));
        assert!(
            tinted.get_pixel(3, 2).0[3] > 0,
            "a cleared pixel shows no tint"
        );

        let base = picture(&comparison.base);
        assert_eq!(
            base.size,
            Size {
                format: "PNG".to_owned(),
                width: 4,
                height: 3
            }
        );
        assert!(!base.first_frame);
        assert!(comparison.has_image());
    }

    /// Versions of other sizes compare no pixels; versions whose bytes differ but whose pixels
    /// do not count none changed.
    #[test]
    fn other_sizes_and_identical_pixels() {
        let small = filled(2, 2, [1, 2, 3, 255]);
        let large = filled(3, 2, [1, 2, 3, 255]);
        let resized = Comparison::of(
            encoded(&small, ImageFormat::Png),
            encoded(&large, ImageFormat::Png),
        );
        assert_eq!(resized.changes, Changes::Sizes);

        // The same pixels as a PNG and as a BMP: other bytes, nothing changed.
        let same = Comparison::of(
            encoded(&small, ImageFormat::Png),
            encoded(&small, ImageFormat::Bmp),
        );
        let Changes::Pixels { changed, .. } = same.changes else {
            panic!("no pixels compared: {:?}", same.changes);
        };
        assert_eq!(changed, 0);
        assert_eq!(picture(&same.head).size.format, "BMP");
    }

    /// An addition or a deletion shows the absent version as absent and compares nothing.
    #[test]
    fn an_absent_version_compares_nothing() {
        let image = filled(2, 2, [9, 9, 9, 255]);
        let added = Comparison::of(Encoded::Absent, encoded(&image, ImageFormat::Jpeg));
        assert_eq!(added.base, Version::Absent);
        assert_eq!(picture(&added.head).size.format, "JPEG");
        assert_eq!(added.changes, Changes::None);
        let deleted = Comparison::of(encoded(&image, ImageFormat::Gif), Encoded::Absent);
        assert_eq!(deleted.head, Version::Absent);
        assert_eq!(picture(&deleted.base).size.format, "GIF");
    }

    /// Bounds hold before anything large is read or decoded: an oversized blob stays unread,
    /// and an image with too many pixels is not decoded but keeps its size for its label.
    #[test]
    fn bounds_refuse_before_decoding() {
        let large = Comparison::of(Encoded::TooLarge(MAX_BYTES + 1), Encoded::Absent);
        assert_eq!(large.base, Version::TooLarge(MAX_BYTES + 1));
        // Neither version is known to be an image, so the file shows as binary.
        assert!(!large.has_image());

        // A BMP's headers, which claim 5000 × 5000 pixels of 24 bits, with no pixels after them.
        let mut huge = b"BM".to_vec();
        for field in [54_u32, 0, 54, 40, 5000, 5000] {
            huge.extend(field.to_le_bytes());
        }
        huge.extend(1_u16.to_le_bytes());
        huge.extend(24_u16.to_le_bytes());
        huge.extend([0; 24]);
        let comparison = Comparison::of(Encoded::Bytes(huge), Encoded::Absent);
        assert_eq!(
            comparison.base,
            Version::TooManyPixels(Size {
                format: "BMP".to_owned(),
                width: 5000,
                height: 5000
            })
        );
        assert!(comparison.has_image());
    }

    /// A large image is fitted within the display edge, keeping its aspect, while its label
    /// keeps the full size.
    #[test]
    fn large_images_fit_the_display_edge() {
        // A BMP, which writes fast, as large as the edge is long.
        let wide = filled(3000, 300, [5, 5, 5, 255]);
        let comparison = Comparison::of(encoded(&wide, ImageFormat::Bmp), Encoded::Absent);
        let base = picture(&comparison.base);
        assert_eq!((base.size.width, base.size.height), (3000, 300));
        assert_eq!(base.raster.dimensions(), (DISPLAY_EDGE, 204));
    }

    /// Content in no image format, and an image that does not decode, each say so; only the
    /// latter is worth a comparison.
    #[test]
    fn non_images_and_broken_images() {
        let text = Comparison::of(
            Encoded::Bytes(b"LOGO\0binary".to_vec()),
            Encoded::Unreadable,
        );
        assert_eq!(text.base, Version::NotImage);
        assert_eq!(text.head, Version::Unreadable(None));
        assert!(!text.has_image());

        let mut png = bytes(&filled(8, 8, [1, 1, 1, 255]), ImageFormat::Png);
        png.truncate(40);
        let broken = Comparison::of(Encoded::Bytes(png), Encoded::Absent);
        assert_eq!(broken.base, Version::Unreadable(Some("PNG".to_owned())));
        assert!(broken.has_image());
    }

    /// A GIF of one frame for each of `colors`, 2 × 2 pixels.
    fn gif(colors: &[[u8; 4]]) -> Encoded {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame};

        let mut bytes = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut bytes);
            for &color in colors {
                let delay = Delay::from_saturating_duration(std::time::Duration::from_millis(100));
                let frame = Frame::from_parts(filled(2, 2, color), 0, 0, delay);
                encoder.encode_frame(frame).unwrap();
            }
        }
        Encoded::Bytes(bytes)
    }

    /// A still GIF and an animation whose first frame is the same image, as when a still image
    /// becomes an animation: only the first frames are compared, and they show no difference,
    /// while both say they were compared by their first frame.
    #[test]
    fn an_animation_compares_its_first_frame() {
        let sky = [142, 202, 230, 255];
        let deep = [33, 158, 188, 255];
        let comparison = Comparison::of(gif(&[sky]), gif(&[sky, deep]));
        assert!(picture(&comparison.base).first_frame);
        let head = picture(&comparison.head);
        assert!(head.first_frame);
        assert_eq!(head.raster.get_pixel(0, 0).0, sky);
        assert!(matches!(
            comparison.changes,
            Changes::Pixels { changed: 0, .. }
        ));
    }

    /// An APNG shows and compares its first animation frame, not the default image shown in
    /// its place by programs that know no animation.
    #[test]
    fn an_apng_shows_its_first_animation_frame() {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(2, 0).unwrap();
            encoder.set_sep_def_img(true).unwrap();
            let mut writer = encoder.write_header().unwrap();
            // The default image, then the animation's two frames.
            for color in [[255, 0, 0, 255], [0, 0, 255, 255], [0, 255, 0, 255]] {
                writer.write_image_data(&color.repeat(4)).unwrap();
            }
        }
        let comparison = Comparison::of(Encoded::Bytes(bytes), Encoded::Absent);
        let base = picture(&comparison.base);
        assert!(base.first_frame);
        assert_eq!(base.raster.get_pixel(0, 0).0, [0, 0, 255, 255]);
    }

    /// A version whose pixels are the base's but whose orientation turns them shows turned, so
    /// the comparison never claims the versions show alike.
    #[test]
    fn orientation_turns_an_image_before_it_is_compared() {
        use image::ImageEncoder as _;
        use image::codecs::png::PngEncoder;

        let base = RgbaImage::from_fn(4, 2, |x, _| {
            Rgba([u8::try_from(x).unwrap() * 60, 0, 0, 255])
        });
        // A TIFF header and one entry: orientation 6, turned a quarter clockwise.
        let exif = [
            b"MM\0\x2a\0\0\0\x08\0\x01".as_slice(),
            &[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0],
            &[0, 0, 0, 0],
        ]
        .concat();
        let mut turned = Vec::new();
        let mut encoder = PngEncoder::new(&mut turned);
        encoder.set_exif_metadata(exif).unwrap();
        encoder
            .write_image(base.as_raw(), 4, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        let comparison = Comparison::of(encoded(&base, ImageFormat::Png), Encoded::Bytes(turned));
        let head = picture(&comparison.head);
        assert_eq!((head.size.width, head.size.height), (2, 4));
        assert_eq!(comparison.changes, Changes::Sizes);
    }

    /// A raster fits its room by its tighter side.
    #[test]
    fn rasters_fit_their_room() {
        let room = Extent {
            width: 100.0,
            height: 30.0,
        };
        // Scales of a quarter and of five, which floats hold exactly.
        assert_eq!(
            fitted(400, 120, room),
            Extent {
                width: 100.0,
                height: 30.0
            }
        );
        assert_eq!(
            fitted(
                4,
                2,
                Extent {
                    width: 20.0,
                    height: 30.0
                }
            ),
            Extent {
                width: 20.0,
                height: 10.0
            }
        );
    }
}
