//! Decoding a wallpaper, and fitting it to a screen.
//!
//! # Alpha
//!
//! Decoded pixels are **premultiplied**. That is the one thing in this module
//! worth knowing before reading it, and it buys two things:
//!
//! - Bilinear sampling is correct. Interpolating straight alpha blends the
//!   colour of fully transparent pixels into their neighbours, which is where
//!   the dark halo around a scaled-down PNG logo comes from.
//! - Compositing onto the background colour is `src + background * (1 - a)`,
//!   with no division anywhere in the per-pixel path.
//!
//! For an opaque photograph -- which is what a wallpaper almost always is --
//! premultiplied and straight are the same bytes, so this costs nothing in the
//! common case.
//!
//! Alpha is honoured rather than discarded, unlike RavenLogin's greeter, which
//! drops it. The greeter is right for a greeter: there is nothing behind its
//! wallpaper. Here there is -- `background` in the config -- and a PNG with a
//! transparent border laid on a chosen colour is a thing people actually want.

use std::fmt;
use std::io::Cursor;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::color::Color;

/// The largest file this will read, before any decoding.
///
/// A wallpaper is a photograph. 64 MiB is far past any real one and well short
/// of anything that would embarrass this process's memory footprint.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// The largest image this will decode, in pixels.
///
/// 64 megapixels is about 8000x8000 -- comfortably more than an 8K panel needs
/// and roughly 256 MiB once it is four bytes a pixel, which is the real
/// ceiling being set here. Both decoders are told about this rather than being
/// allowed to allocate first and be checked afterwards.
pub const MAX_PIXELS: usize = 64 * 1_000_000;

/// A decoded image, premultiplied, in the canvas layout.
pub struct Image {
    width: u32,
    height: u32,
    /// `width * height * 4` bytes of `[B, G, R, A]`, premultiplied.
    pixels: Vec<u8>,
}

/// Hand-written: the derived one would try to format several megabytes of
/// pixels into a log line.
impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Image {
    /// Read and decode the file at `path`.
    ///
    /// The format is decided by the file's first bytes and not by its
    /// extension. Partly because an extension is a claim anybody can get wrong
    /// -- a `.jpg` that is really a PNG is a common enough accident that it is
    /// worth not caring about -- and partly because dispatching a parser on a
    /// filename is how a parser ends up being handed a file the caller did not
    /// think it was handing it.
    pub fn load(path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path)
            .with_context(|| format!("cannot stat {}", path.display()))?
            .len();
        if size > MAX_FILE_BYTES {
            bail!(
                "{} is {size} bytes, past the {MAX_FILE_BYTES}-byte limit for a wallpaper",
                path.display()
            );
        }

        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let image = Self::decode(&bytes)
            .with_context(|| format!("cannot decode {} as a wallpaper", path.display()))?;

        tracing::debug!(
            path = %path.display(),
            width = image.width,
            height = image.height,
            "decoded a wallpaper"
        );
        Ok(image)
    }

    /// Decode bytes already in hand, dispatching on the magic number.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        const JPEG_MAGIC: &[u8] = &[0xFF, 0xD8, 0xFF];

        if bytes.starts_with(PNG_MAGIC) {
            decode_png(bytes)
        } else if bytes.starts_with(JPEG_MAGIC) {
            decode_jpeg(bytes)
        } else {
            bail!("not a PNG or a JPEG");
        }
    }

    /// Build one directly from premultiplied canvas-order bytes.
    ///
    /// For tests and for `--preview`; nothing on the decode path uses it.
    pub fn from_bgra(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self> {
        check_dimensions(width, height)?;
        let wanted = width as usize * height as usize * 4;
        if pixels.len() != wanted {
            bail!("{} bytes is not a {width}x{height} image", pixels.len());
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Whether the file has anything to composite. An opaque image can take a
    /// faster path and, more usefully, tells the caller the background colour
    /// will never be seen.
    #[must_use]
    pub fn is_opaque(&self) -> bool {
        self.pixels.chunks_exact(4).all(|p| p[3] == 0xFF)
    }

    /// Fit this image to a `width` x `height` surface, over `background`.
    ///
    /// The result is `width * height * 4` opaque bytes, ready to
    /// [`crate::Canvas::blit`]. This is the expensive operation in the whole
    /// engine -- a few million bilinear samples -- and it is why callers cache
    /// it: it is redone when the image changes or the screen is resized, and
    /// on a static wallpaper that means twice in a session.
    #[must_use]
    pub fn fit_into(&self, width: i32, height: i32, fit: Fit, background: Color) -> Vec<u8> {
        let (dst_w, dst_h) = (width.max(0) as usize, height.max(0) as usize);
        let mut out = vec![0u8; dst_w * dst_h * 4];
        if dst_w == 0 || dst_h == 0 || self.width == 0 || self.height == 0 {
            return out;
        }

        let map = Mapping::for_fit(fit, self.width, self.height, dst_w, dst_h);
        let ground = background.with_alpha(0xFF);
        let (gb, gg, gr) = (
            f32::from(ground.blue()),
            f32::from(ground.green()),
            f32::from(ground.red()),
        );

        for y in 0..dst_h {
            let sy = map.oy + (y as f32 + 0.5) * map.sy;
            for x in 0..dst_w {
                let sx = map.ox + (x as f32 + 0.5) * map.sx;
                let i = (y * dst_w + x) * 4;

                let Some([b, g, r, a]) = self.sample(sx, sy, map.tile) else {
                    // Outside the image: `contain`'s letterbox, or `center` on
                    // a screen larger than the picture.
                    out[i] = ground.blue();
                    out[i + 1] = ground.green();
                    out[i + 2] = ground.red();
                    out[i + 3] = 0xFF;
                    continue;
                };

                // Premultiplied source over an opaque ground. No division:
                // that is what premultiplying bought.
                let transmit = 1.0 - a / 255.0;
                out[i] = (b + gb * transmit) as u8;
                out[i + 1] = (g + gg * transmit) as u8;
                out[i + 2] = (r + gr * transmit) as u8;
                out[i + 3] = 0xFF;
            }
        }
        out
    }

    /// Bilinear sample in premultiplied `[B, G, R, A]`, unrounded.
    ///
    /// `None` means the point is off the image, which only happens when `tile`
    /// is false. Bilinear and not nearest because the common case is a
    /// 1920x1080 photo on a panel that is not 1920x1080, and nearest-neighbour
    /// on a downscale is where the aliasing on a diagonal comes from. It is
    /// not a box filter, so a very large downscale still aliases; that is a
    /// trade for one pass over the destination rather than one over the
    /// source.
    fn sample(&self, x: f32, y: f32, tile: bool) -> Option<[f32; 4]> {
        let (w, h) = (self.width as i32, self.height as i32);
        if !tile && (x < 0.0 || y < 0.0 || x >= w as f32 || y >= h as f32) {
            return None;
        }

        // Half a pixel back, so `x0` is the sample to the left of the point
        // rather than the one containing it.
        let fx = x - 0.5;
        let fy = y - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);

        let wrap = |v: i32, limit: i32| -> usize {
            if tile {
                v.rem_euclid(limit) as usize
            } else {
                v.clamp(0, limit - 1) as usize
            }
        };
        let (x0, x1) = (wrap(x0, w), wrap(x0 + 1, w));
        let (y0, y1) = (wrap(y0, h), wrap(y0 + 1, h));

        let at = |px: usize, py: usize, c: usize| -> f32 {
            f32::from(self.pixels[(py * self.width as usize + px) * 4 + c])
        };
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;

        let mut out = [0.0f32; 4];
        for (c, channel) in out.iter_mut().enumerate() {
            let top = lerp(at(x0, y0, c), at(x1, y0, c), tx);
            let bottom = lerp(at(x0, y1, c), at(x1, y1, c), tx);
            *channel = lerp(top, bottom, ty);
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------

/// How an image is placed on a screen it does not exactly match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Scale until it fills the screen and crop the overflowing axis, from the
    /// centre. The default, because a letterboxed wallpaper looks like a
    /// mistake and a stretched one looks like a bug.
    #[default]
    Cover,
    /// Scale until all of it is visible and fill the rest with the background
    /// colour. For a picture whose edges matter.
    Contain,
    /// Scale each axis independently, distorting it. Included because it is
    /// what somebody occasionally wants for a texture, and excluded from the
    /// documentation's recommendations for every other case.
    Stretch,
    /// One image pixel to one screen pixel, centred, cropped or surrounded by
    /// the background colour. Device pixels, not logical ones -- a pixel-art
    /// wallpaper asking for `center` wants exactly its own pixels.
    Center,
    /// Repeat from the top-left corner. For a pattern rather than a picture.
    Tile,
}

impl Fit {
    /// Every value, in the order the documentation lists them.
    pub const ALL: [Self; 5] = [
        Self::Cover,
        Self::Contain,
        Self::Stretch,
        Self::Center,
        Self::Tile,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cover => "cover",
            Self::Contain => "contain",
            Self::Stretch => "stretch",
            Self::Center => "center",
            Self::Tile => "tile",
        }
    }

    /// Whether this fit can leave part of the screen showing the background
    /// colour. `cover`, `stretch` and `tile` never can.
    #[must_use]
    pub const fn can_letterbox(self) -> bool {
        matches!(self, Self::Contain | Self::Center)
    }
}

impl FromStr for Fit {
    type Err = ParseFitError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|f| f.name().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| ParseFitError(s.to_string()))
    }
}

impl fmt::Display for Fit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that is not one of the [`Fit`] modes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFitError(String);

impl fmt::Display for ParseFitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = Fit::ALL.iter().map(|f| f.name()).collect();
        write!(
            f,
            "{:?} is not a fit mode; expected one of {}",
            self.0,
            names.join(", ")
        )
    }
}

impl std::error::Error for ParseFitError {}

/// Destination pixel to source pixel, as a scale and an origin.
///
/// Every fit mode is this same affine map with different numbers in it, which
/// is why there is one sampling loop rather than five.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mapping {
    /// Source pixels per destination pixel, per axis.
    sx: f32,
    sy: f32,
    /// The source coordinate the destination's origin maps to. Negative for
    /// `contain`, which is what puts the letterbox outside the image.
    ox: f32,
    oy: f32,
    tile: bool,
}

impl Mapping {
    fn for_fit(fit: Fit, src_w: u32, src_h: u32, dst_w: usize, dst_h: usize) -> Self {
        let (sw, sh) = (src_w as f32, src_h as f32);
        let (dw, dh) = (dst_w as f32, dst_h as f32);

        // `cover` and `contain` differ by exactly one function: the larger
        // ratio leaves no gap, the smaller one leaves no crop.
        let uniform = |scale: f32| Self {
            sx: 1.0 / scale,
            sy: 1.0 / scale,
            ox: (sw - dw / scale) / 2.0,
            oy: (sh - dh / scale) / 2.0,
            tile: false,
        };

        match fit {
            Fit::Cover => uniform((dw / sw).max(dh / sh)),
            Fit::Contain => uniform((dw / sw).min(dh / sh)),
            Fit::Center => uniform(1.0),
            Fit::Stretch => Self {
                sx: sw / dw,
                sy: sh / dh,
                ox: 0.0,
                oy: 0.0,
                tile: false,
            },
            Fit::Tile => Self {
                sx: 1.0,
                sy: 1.0,
                ox: 0.0,
                oy: 0.0,
                tile: true,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

fn decode_png(bytes: &[u8]) -> Result<Image> {
    // A `Cursor`, because both decoders want `BufRead + Seek` rather than a
    // slice. Nothing is copied: it is a cursor over the bytes already in hand.
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // Ask the decoder to normalise the awkward cases -- palettes, 1/2/4-bit
    // depths, 16-bit channels, missing alpha -- so the only outputs left to
    // handle below are 8-bit RGBA and 8-bit grey+alpha.
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    decoder.set_limits(png::Limits {
        bytes: MAX_PIXELS * 4,
    });

    let mut reader = decoder.read_info().context("bad PNG header")?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    check_dimensions(width, height)?;

    let mut buffer = vec![0u8; reader.output_buffer_size().context("PNG is too large")?];
    let frame = reader.next_frame(&mut buffer).context("bad PNG data")?;
    let channels = match frame.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::GrayscaleAlpha => 2,
        other => bail!("unsupported PNG colour type {other:?}"),
    };

    let pixels = to_canvas_order(&buffer[..frame.buffer_size()], width, height, channels)?;
    Ok(Image {
        width,
        height,
        pixels,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<Image> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    // The dimension caps are set before the header is read, so an oversized
    // image is refused by the decoder rather than allocated and then rejected.
    let options = DecoderOptions::default()
        .jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(MAX_PIXELS)
        .set_max_height(MAX_PIXELS);

    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(Cursor::new(bytes), options);
    decoder
        .decode_headers()
        .map_err(|e| anyhow::anyhow!("bad JPEG header: {e}"))?;
    let (width, height) = decoder
        .dimensions()
        .context("the JPEG header carries no dimensions")?;
    let (width, height) = (
        u32::try_from(width).context("absurd JPEG width")?,
        u32::try_from(height).context("absurd JPEG height")?,
    );
    check_dimensions(width, height)?;

    let decoded = decoder
        .decode()
        .map_err(|e| anyhow::anyhow!("bad JPEG data: {e}"))?;

    // A greyscale JPEG comes back as one channel even having asked for RGB,
    // because the requested colourspace is a request and not a conversion.
    let channels = match decoder.output_colorspace() {
        Some(ColorSpace::RGB) => 3,
        Some(ColorSpace::Luma) => 1,
        other => bail!("unsupported JPEG colourspace {other:?}"),
    };

    let pixels = to_canvas_order(&decoded, width, height, channels)?;
    Ok(Image {
        width,
        height,
        pixels,
    })
}

fn check_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("the image is {width}x{height}");
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .context("the image dimensions overflow")?;
    if pixels > MAX_PIXELS {
        bail!("the image is {width}x{height}, past the {MAX_PIXELS}-pixel limit");
    }
    Ok(())
}

/// Convert decoder output to premultiplied `[B, G, R, A]`.
///
/// `channels` is what the decoder produced per pixel: 4 for RGBA, 3 for RGB,
/// 2 for grey+alpha, 1 for grey.
fn to_canvas_order(src: &[u8], width: u32, height: u32, channels: usize) -> Result<Vec<u8>> {
    let count = (width as usize) * (height as usize);
    let wanted = count * channels;
    if src.len() < wanted {
        bail!(
            "the decoder produced {} bytes for a {width}x{height} image needing {wanted}",
            src.len()
        );
    }

    let mut out = vec![0u8; count * 4];
    for i in 0..count {
        let p = i * channels;
        let (r, g, b, a) = match channels {
            1 => (src[p], src[p], src[p], 0xFF),
            2 => (src[p], src[p], src[p], src[p + 1]),
            3 => (src[p], src[p + 1], src[p + 2], 0xFF),
            _ => (src[p], src[p + 1], src[p + 2], src[p + 3]),
        };

        let o = i * 4;
        if a == 0xFF {
            // The overwhelmingly common case, and exact: premultiplying by 1
            // is a copy, and routing it through the multiply below would round
            // every channel of every opaque photograph for nothing.
            out[o] = b;
            out[o + 1] = g;
            out[o + 2] = r;
        } else {
            // `x * a / 255`, rounded, without a division: the usual
            // `(v + 128 + (v >> 8)) >> 8` trick, exact for all 8-bit inputs.
            let premultiply = |v: u8| -> u8 {
                let t = u32::from(v) * u32::from(a) + 128;
                ((t + (t >> 8)) >> 8) as u8
            };
            out[o] = premultiply(b);
            out[o + 1] = premultiply(g);
            out[o + 2] = premultiply(r);
        }
        out[o + 3] = a;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2x2 image: red, green / blue, white. Opaque, so premultiplied and
    /// straight are the same bytes.
    fn checker() -> Image {
        #[rustfmt::skip]
        let pixels = vec![
            0x00, 0x00, 0xFF, 0xFF,   0x00, 0xFF, 0x00, 0xFF,
            0xFF, 0x00, 0x00, 0xFF,   0xFF, 0xFF, 0xFF, 0xFF,
        ];
        Image {
            width: 2,
            height: 2,
            pixels,
        }
    }

    /// `(r, g, b)` at a point of a fitted buffer.
    fn at(out: &[u8], width: usize, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * width + x) * 4;
        (out[i + 2], out[i + 1], out[i])
    }

    const GROUND: Color = Color::from_argb(0xFF16_161F);

    // -- channel order and premultiplication --------------------------------

    #[test]
    fn rgb_becomes_bgra() {
        assert_eq!(
            to_canvas_order(&[0x11, 0x22, 0x33], 1, 1, 3).unwrap(),
            vec![0x33, 0x22, 0x11, 0xFF]
        );
    }

    #[test]
    fn grey_expands_to_three_channels() {
        assert_eq!(
            to_canvas_order(&[0x7F], 1, 1, 1).unwrap(),
            vec![0x7F, 0x7F, 0x7F, 0xFF]
        );
    }

    #[test]
    fn an_opaque_pixel_is_copied_rather_than_multiplied() {
        // Exactness matters: every pixel of every photograph takes this path,
        // and a rounding error here would be a whole-image shift.
        let out = to_canvas_order(&[0x01, 0x02, 0x03, 0xFF], 1, 1, 4).unwrap();
        assert_eq!(out, vec![0x03, 0x02, 0x01, 0xFF]);
    }

    #[test]
    fn a_translucent_pixel_is_premultiplied() {
        let out = to_canvas_order(&[0xFF, 0xFF, 0xFF, 0x80], 1, 1, 4).unwrap();
        assert_eq!(out[3], 0x80);
        assert_eq!(out[0], 0x80, "white at half alpha premultiplies to half");
    }

    #[test]
    fn a_fully_transparent_pixel_keeps_no_colour() {
        let out = to_canvas_order(&[0xFF, 0x00, 0x00, 0x00], 1, 1, 4).unwrap();
        assert_eq!(
            out,
            vec![0, 0, 0, 0],
            "transparent must not smear its colour"
        );
    }

    #[test]
    fn a_short_buffer_is_an_error_rather_than_a_panic() {
        assert!(to_canvas_order(&[0x00, 0x11], 4, 4, 3).is_err());
    }

    // -- fitting ------------------------------------------------------------

    #[test]
    fn fitting_produces_exactly_the_requested_size() {
        for (w, h) in [(1, 1), (7, 3), (1920, 1080), (100, 400)] {
            for fit in Fit::ALL {
                let out = checker().fit_into(w, h, fit, GROUND);
                assert_eq!(out.len(), (w * h * 4) as usize, "{fit} at {w}x{h}");
            }
        }
    }

    #[test]
    fn fitting_leaves_every_pixel_opaque() {
        for fit in Fit::ALL {
            let out = checker().fit_into(16, 9, fit, GROUND);
            assert!(out.chunks_exact(4).all(|p| p[3] == 0xFF), "{fit}");
        }
    }

    /// Cover-scaling onto a surface of a different aspect ratio must crop, not
    /// squash. A 2x2 checker on a wide surface keeps its left half red-ish and
    /// its right half green-ish; a squashed one would not.
    #[test]
    fn cover_crops_rather_than_distorting() {
        let out = checker().fit_into(64, 8, Fit::Cover, GROUND);
        let (lr, lg, _) = at(&out, 64, 0, 4);
        let (rr, rg, _) = at(&out, 64, 63, 4);
        assert!(lr > lg, "the left edge should still be red-ish");
        assert!(rg > rr, "the right edge should still be green-ish");
    }

    #[test]
    fn cover_never_shows_the_background() {
        let out = checker().fit_into(64, 8, Fit::Cover, GROUND);
        assert!(
            !out.chunks_exact(4).any(|p| p[..3] == GROUND.to_bgra()[..3]),
            "cover must fill the screen"
        );
        assert!(!Fit::Cover.can_letterbox());
    }

    #[test]
    fn contain_letterboxes_with_the_background_colour() {
        // A square image on a wide screen: the left and right columns are the
        // ground, the middle is the picture.
        let out = checker().fit_into(64, 8, Fit::Contain, GROUND);
        assert_eq!(at(&out, 64, 0, 4), (0x16, 0x16, 0x1F), "left bar");
        assert_eq!(at(&out, 64, 63, 4), (0x16, 0x16, 0x1F), "right bar");
        assert_ne!(
            at(&out, 64, 32, 4),
            (0x16, 0x16, 0x1F),
            "the middle is the picture"
        );
        assert!(Fit::Contain.can_letterbox());
    }

    #[test]
    fn stretch_reaches_every_corner_of_the_source() {
        // Each corner of the destination is the corresponding source corner:
        // red, green / blue, white.
        let out = checker().fit_into(32, 32, Fit::Stretch, GROUND);
        let (r, g, b) = at(&out, 32, 0, 0);
        assert!(r > g && r > b, "top-left should be red, got {r} {g} {b}");
        let (r, g, b) = at(&out, 32, 31, 31);
        assert!(
            r > 0xE0 && g > 0xE0 && b > 0xE0,
            "bottom-right should be white"
        );
    }

    #[test]
    fn center_maps_one_source_pixel_to_one_screen_pixel() {
        // A 2x2 image on an 8x8 screen sits in the middle two rows and
        // columns; everything else is the ground.
        let out = checker().fit_into(8, 8, Fit::Center, GROUND);
        assert_eq!(at(&out, 8, 0, 0), (0x16, 0x16, 0x1F));
        assert_eq!(
            at(&out, 8, 3, 3),
            (0xFF, 0x00, 0x00),
            "the source's top-left, unscaled"
        );
        assert_eq!(
            at(&out, 8, 4, 4),
            (0xFF, 0xFF, 0xFF),
            "the source's bottom-right"
        );
    }

    #[test]
    fn tile_repeats_and_never_shows_the_background() {
        let out = checker().fit_into(8, 8, Fit::Tile, GROUND);
        // Every second pixel repeats the one two along.
        for y in 0..8 {
            for x in 0..6 {
                assert_eq!(at(&out, 8, x, y), at(&out, 8, x + 2, y), "at {x},{y}");
            }
        }
    }

    /// A transparent image over a chosen colour is the reason alpha is kept
    /// rather than dropped. This is that, at its clearest: a fully transparent
    /// source must come out as exactly the background.
    #[test]
    fn a_transparent_image_composites_onto_the_background() {
        let clear = Image::from_bgra(1, 1, vec![0, 0, 0, 0]).unwrap();
        let out = clear.fit_into(4, 4, Fit::Cover, GROUND);
        assert!(
            out.chunks_exact(4).all(|p| p == GROUND.to_bgra()),
            "got {:?}",
            &out[..4]
        );
        assert!(!clear.is_opaque());
    }

    #[test]
    fn a_half_transparent_image_lands_between_itself_and_the_background() {
        // White at half alpha, premultiplied, over a near-black ground.
        let half = Image::from_bgra(1, 1, vec![0x80, 0x80, 0x80, 0x80]).unwrap();
        let (r, _, _) = at(&half.fit_into(2, 2, Fit::Cover, GROUND), 2, 0, 0);
        assert!(r > 0x16 && r < 0xFF, "expected a mix, got {r:#04x}");
    }

    #[test]
    fn an_opaque_image_says_so() {
        assert!(checker().is_opaque());
    }

    #[test]
    fn fitting_a_zero_sized_surface_returns_nothing_and_does_not_panic() {
        assert!(checker().fit_into(0, 0, Fit::Cover, GROUND).is_empty());
        assert!(checker().fit_into(-4, 10, Fit::Tile, GROUND).is_empty());
    }

    // -- the mapping itself -------------------------------------------------

    #[test]
    fn cover_and_contain_differ_only_in_which_ratio_wins() {
        // A 2:1 source on a 1:1 screen. Cover takes the taller scale and
        // crops the width; contain takes the shorter and letterboxes it.
        let cover = Mapping::for_fit(Fit::Cover, 200, 100, 100, 100);
        let contain = Mapping::for_fit(Fit::Contain, 200, 100, 100, 100);
        assert!(cover.ox > 0.0, "cover crops horizontally");
        assert_eq!(cover.oy, 0.0);
        assert!(contain.oy < 0.0, "contain letterboxes vertically");
        assert_eq!(contain.ox, 0.0);
    }

    #[test]
    fn only_tile_wraps() {
        for fit in Fit::ALL {
            let map = Mapping::for_fit(fit, 8, 8, 16, 16);
            assert_eq!(map.tile, fit == Fit::Tile, "{fit}");
        }
    }

    // -- fit names ----------------------------------------------------------

    #[test]
    fn fit_names_round_trip_and_ignore_case() {
        for fit in Fit::ALL {
            assert_eq!(fit.to_string().parse::<Fit>().unwrap(), fit);
            assert_eq!(fit.name().to_uppercase().parse::<Fit>().unwrap(), fit);
        }
        assert_eq!(Fit::default(), Fit::Cover);
    }

    #[test]
    fn an_unknown_fit_names_the_ones_that_exist() {
        let error = "squish".parse::<Fit>().unwrap_err().to_string();
        assert!(error.contains("cover"), "{error}");
        assert!(error.contains("squish"), "{error}");
    }

    // -- decoding -----------------------------------------------------------

    #[test]
    fn a_file_that_is_not_an_image_is_refused() {
        assert!(Image::decode(b"this is not a wallpaper").is_err());
        assert!(Image::decode(&[]).is_err());
    }

    /// A truncated file has the right magic and nothing else. It must come
    /// back as an error, because the alternative -- a panic -- is a wallpaper
    /// daemon that dies over a half-copied file appearing in a slideshow
    /// directory, which is a thing that happens routinely.
    #[test]
    fn a_truncated_png_is_an_error_rather_than_a_panic() {
        let mut truncated = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        truncated.extend_from_slice(&[0x00; 16]);
        assert!(Image::decode(&truncated).is_err());
    }

    #[test]
    fn a_truncated_jpeg_is_an_error_rather_than_a_panic() {
        assert!(Image::decode(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]).is_err());
    }

    #[test]
    fn absurd_dimensions_are_refused() {
        assert!(check_dimensions(0, 100).is_err());
        assert!(check_dimensions(100, 0).is_err());
        assert!(check_dimensions(60_000, 60_000).is_err());
        assert!(check_dimensions(1920, 1080).is_ok());
    }

    #[test]
    fn a_missing_file_is_an_error_naming_it() {
        let error = Image::load(Path::new("/nonexistent/wallpaper.png")).unwrap_err();
        assert!(format!("{error:#}").contains("wallpaper.png"));
    }

    /// A real PNG, round-tripped through the encoder this crate already
    /// depends on for `--preview`. This is the only test that exercises the
    /// decoder against bytes rather than against a hand-built buffer.
    #[test]
    fn a_real_png_decodes_to_the_pixels_it_was_written_from() {
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 2, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            #[rustfmt::skip]
            let rgba = [
                0xFF, 0x00, 0x00, 0xFF,   0x00, 0xFF, 0x00, 0xFF,
                0x00, 0x00, 0xFF, 0xFF,   0xFF, 0xFF, 0xFF, 0xFF,
            ];
            writer.write_image_data(&rgba).unwrap();
        }

        let image = Image::decode(&encoded).unwrap();
        assert_eq!((image.width(), image.height()), (2, 2));
        assert!(image.is_opaque());
        assert_eq!(
            &image.pixels[..4],
            &[0x00, 0x00, 0xFF, 0xFF],
            "top-left is red"
        );
    }
}
