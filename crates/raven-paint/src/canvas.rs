//! A borrowed buffer, and the operations the wallpaper engine draws with.
//!
//! The wallpaper is drawn on the CPU. That is a decision rather than a
//! shortcut, and it is the same one RavenLogin's greeter made: using the GPU
//! would mean this process links EGL, GLES and a DRM driver -- a large amount
//! of C, resident for the whole life of a session -- to composite a picture
//! that changes at most thirty times a second and usually not at all. huginn
//! already holds the GPU. This fills a rectangle of memory and hands it over.
//!
//! The cost of that decision is the reason [`crate::Field`] exists: a
//! full-resolution procedural scene at 4K is genuinely too much arithmetic for
//! one core, so the scenes draw small and are upscaled. Everything else here
//! is either a memory-bandwidth operation ([`Canvas::blit`],
//! [`Canvas::crossfade`]) or happens once ([`Canvas::gradient`]).
//!
//! The canvas is opaque. Everything is composited onto a filled ground, so the
//! alpha channel is `0xFF` everywhere by the time the compositor sees it and
//! premultiplication never comes into it.

use crate::color::Color;

/// A borrowed `wl_shm` buffer, with drawing operations on top.
#[derive(Debug)]
pub struct Canvas<'a> {
    data: &'a mut [u8],
    width: i32,
    height: i32,
}

impl<'a> Canvas<'a> {
    /// Wrap a buffer. `data` must be at least `width * height * 4` bytes.
    ///
    /// "At least", not "exactly": an shm pool is allocated for the largest
    /// screen seen so far and handed out for smaller ones, so an oversized
    /// slice is the normal case rather than a mistake.
    #[must_use]
    pub fn new(data: &'a mut [u8], width: i32, height: i32) -> Self {
        debug_assert!(
            crate::buffer_len(width, height).is_some_and(|n| data.len() >= n),
            "a {} byte buffer is too small for {width}x{height}",
            data.len()
        );
        Self {
            data,
            width: width.max(0),
            height: height.max(0),
        }
    }

    #[must_use]
    pub const fn width(&self) -> i32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> i32 {
        self.height
    }

    /// The pixels, for a caller that is about to encode them.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let len = crate::buffer_len(self.width, self.height).unwrap_or(0);
        &self.data[..len.min(self.data.len())]
    }

    /// Fill every pixel with one colour, opaque.
    ///
    /// The alpha of `color` is ignored rather than honoured: this is the
    /// ground, and there is nothing behind a wallpaper to blend it onto.
    pub fn fill(&mut self, color: Color) {
        let bytes = color.with_alpha(0xFF).to_bgra();
        for pixel in self.rows_mut() {
            pixel.copy_from_slice(&bytes);
        }
    }

    /// Fill with a vertical gradient, `top` to `bottom`.
    ///
    /// The channel values are dithered before they are truncated -- see
    /// [`crate::dither`] for why a gradient without that produces the banding
    /// it exists to prevent.
    pub fn gradient(&mut self, top: Color, bottom: Color) {
        let span = (self.height - 1).max(1) as f32;
        for y in 0..self.height {
            let t = y as f32 / span;
            let lerp = |a: u8, b: u8| f32::from(a) + (f32::from(b) - f32::from(a)) * t;
            let (bf, gf, rf) = (
                lerp(top.blue(), bottom.blue()),
                lerp(top.green(), bottom.green()),
                lerp(top.red(), bottom.red()),
            );

            let Some(row) = self.row_mut(y) else { continue };
            for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
                let x = x as i32;
                pixel[0] = crate::dither::quantize(bf, x, y);
                pixel[1] = crate::dither::quantize(gf, x, y);
                pixel[2] = crate::dither::quantize(rf, x, y);
                pixel[3] = 0xFF;
            }
        }
    }

    /// Composite one pixel, source-over.
    ///
    /// Out-of-bounds coordinates are dropped rather than clamped. A shape
    /// partly off the edge should be clipped, not smeared along it, and the
    /// callers rely on being able to iterate a bounding box that hangs off the
    /// screen.
    pub fn blend(&mut self, x: i32, y: i32, color: Color, coverage: f32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let alpha = f32::from(color.alpha()) / 255.0 * coverage.clamp(0.0, 1.0);
        if alpha <= 0.0 {
            return;
        }

        let index = ((y as usize * self.width as usize) + x as usize) * 4;
        let Some(pixel) = self.data.get_mut(index..index + 4) else {
            return;
        };
        let mix = |dst: u8, src: u8| -> u8 {
            (f32::from(src) * alpha + f32::from(dst) * (1.0 - alpha)) as u8
        };
        pixel[0] = mix(pixel[0], color.blue());
        pixel[1] = mix(pixel[1], color.green());
        pixel[2] = mix(pixel[2], color.red());
        pixel[3] = 0xFF;
    }

    /// Write one pixel, opaque, with no blending.
    ///
    /// For a caller that is painting the ground rather than compositing onto
    /// it -- [`crate::Field::upscale_into`] is the only one. Going through
    /// [`Canvas::blend`] instead would spend a float multiply and a read on
    /// every pixel of every frame of an animated wallpaper to composite
    /// something at full coverage.
    pub fn set_opaque(&mut self, x: i32, y: i32, red: u8, green: u8, blue: u8) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let index = ((y as usize * self.width as usize) + x as usize) * 4;
        let Some(pixel) = self.data.get_mut(index..index + 4) else {
            return;
        };
        pixel[0] = blue;
        pixel[1] = green;
        pixel[2] = red;
        pixel[3] = 0xFF;
    }

    /// Copy a prepared buffer of exactly this canvas's size over the whole
    /// canvas.
    ///
    /// This is the entire cost of a static wallpaper frame, and it is why
    /// scaling is cached rather than redone: an image already fitted to the
    /// screen is one `copy_from_slice` away from being on it.
    ///
    /// A source of the wrong length is ignored, with a warning. The
    /// alternative -- copying what fits -- would put a torn image on screen,
    /// which reads as a compositor bug rather than as the caller's mistake it
    /// actually is.
    pub fn blit(&mut self, source: &[u8]) {
        let len = crate::buffer_len(self.width, self.height).unwrap_or(0);
        if source.len() < len {
            tracing::warn!(
                have = source.len(),
                want = len,
                "refusing to blit a buffer that is the wrong size for the surface"
            );
            return;
        }
        self.data[..len].copy_from_slice(&source[..len]);
    }

    /// Mix two prepared buffers of this canvas's size, `t` from `first` to
    /// `second`, and write the result.
    ///
    /// This is the crossfade between two slideshow images, and it is the only
    /// per-frame full-screen arithmetic a non-animated wallpaper ever does --
    /// for the second or so a transition lasts, and never otherwise.
    ///
    /// Fixed point rather than float: 257 discrete steps is far more than a
    /// fade of a few hundred milliseconds can show, and it keeps this to an
    /// integer multiply-add per channel. The weights are chosen so that
    /// `t = 0.0` and `t = 1.0` reproduce their input byte-for-byte -- a fade
    /// that ends one level off its destination leaves a visible step when the
    /// transition is torn down.
    pub fn crossfade(&mut self, first: &[u8], second: &[u8], t: f32) {
        let len = crate::buffer_len(self.width, self.height).unwrap_or(0);
        if first.len() < len || second.len() < len {
            tracing::warn!("refusing to crossfade buffers that are the wrong size");
            return;
        }

        let second_weight = (t.clamp(0.0, 1.0) * 256.0).round() as u32;
        let first_weight = 256 - second_weight;
        for ((out, &a), &b) in self.data[..len]
            .iter_mut()
            .zip(&first[..len])
            .zip(&second[..len])
        {
            *out = ((u32::from(a) * first_weight + u32::from(b) * second_weight) >> 8) as u8;
        }
    }

    /// Darken the whole canvas towards `color` by its alpha.
    ///
    /// A scrim, for the case where something legible has to sit on top of a
    /// photograph nobody chose for its contrast. The wallpaper engine does not
    /// use this itself -- there is nothing on top of a wallpaper -- but a
    /// scene may, and `--preview` composites a caption over its output.
    pub fn scrim(&mut self, color: Color) {
        let alpha = u32::from(color.alpha());
        if alpha == 0 {
            return;
        }
        let keep = 255 - alpha;
        let (b, g, r) = (
            u32::from(color.blue()) * alpha,
            u32::from(color.green()) * alpha,
            u32::from(color.red()) * alpha,
        );
        for pixel in self.rows_mut() {
            pixel[0] = ((u32::from(pixel[0]) * keep + b) / 255) as u8;
            pixel[1] = ((u32::from(pixel[1]) * keep + g) / 255) as u8;
            pixel[2] = ((u32::from(pixel[2]) * keep + r) / 255) as u8;
            pixel[3] = 0xFF;
        }
    }

    /// One row of pixels, or `None` if `y` is off the canvas.
    fn row_mut(&mut self, y: i32) -> Option<&mut [u8]> {
        if y < 0 || y >= self.height {
            return None;
        }
        let stride = self.width as usize * 4;
        let start = y as usize * stride;
        self.data.get_mut(start..start + stride)
    }

    /// Every pixel of the canvas, as a four-byte chunk.
    fn rows_mut(&mut self) -> impl Iterator<Item = &mut [u8]> {
        let len = crate::buffer_len(self.width, self.height).unwrap_or(0);
        let len = len.min(self.data.len());
        self.data[..len].chunks_exact_mut(4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(width: i32, height: i32) -> Vec<u8> {
        vec![0; (width * height * 4) as usize]
    }

    /// `(r, g, b, a)` at a point, unpacked out of the little-endian layout.
    fn pixel(data: &[u8], width: i32, x: i32, y: i32) -> (u8, u8, u8, u8) {
        let i = ((y * width + x) * 4) as usize;
        (data[i + 2], data[i + 1], data[i], data[i + 3])
    }

    const ACCENT: Color = Color::from_argb(0xFF7A_A2F7);
    const BACKDROP: Color = Color::from_argb(0xFF16_161F);

    #[test]
    fn a_fill_covers_every_pixel_opaquely() {
        let mut data = buffer(8, 4);
        Canvas::new(&mut data, 8, 4).fill(ACCENT);
        assert!(data.chunks_exact(4).all(|p| p == [0xF7, 0xA2, 0x7A, 0xFF]));
    }

    /// A translucent colour used as the ground still lands opaque. There is
    /// nothing behind a wallpaper, so a half-alpha fill that left alpha at
    /// 0x80 would hand the compositor a surface it composites onto whatever
    /// happened to be in the pool.
    #[test]
    fn a_fill_ignores_the_alpha_it_is_given() {
        let mut data = buffer(2, 2);
        Canvas::new(&mut data, 2, 2).fill(ACCENT.with_alpha(0x40));
        assert_eq!(pixel(&data, 2, 0, 0).3, 0xFF);
    }

    #[test]
    fn the_gradient_runs_top_to_bottom_and_is_opaque() {
        let mut data = buffer(4, 16);
        Canvas::new(&mut data, 4, 16).gradient(BACKDROP, Color::from_argb(0xFF0D_0D14));
        let top = pixel(&data, 4, 0, 0);
        let bottom = pixel(&data, 4, 0, 15);
        // Within one level: dithering perturbs each channel by up to half a
        // step either way, which is the entire point of it.
        assert!(top.0.abs_diff(0x16) <= 1, "top was {}", top.0);
        assert!(bottom.0.abs_diff(0x0D) <= 1, "bottom was {}", bottom.0);
        assert!(top.0 > bottom.0, "the gradient should darken downwards");
        assert_eq!((top.3, bottom.3), (0xFF, 0xFF));
    }

    /// The dither has to actually vary within a row, or it is not dithering.
    #[test]
    fn the_gradient_dithers_across_a_row() {
        let mut data = buffer(64, 64);
        Canvas::new(&mut data, 64, 64).gradient(Color::rgb(0, 0, 0), Color::rgb(9, 9, 9));
        let row: Vec<u8> = (0..64).map(|x| pixel(&data, 64, x, 30).0).collect();
        assert!(
            row.iter().any(|&v| v != row[0]),
            "a shallow gradient should stipple rather than band"
        );
    }

    #[test]
    fn blending_is_source_over_and_leaves_the_canvas_opaque() {
        let mut data = buffer(4, 4);
        let mut canvas = Canvas::new(&mut data, 4, 4);
        canvas.fill(BACKDROP);
        canvas.blend(1, 1, Color::rgb(255, 255, 255).with_alpha(0x80), 1.0);

        let (r, _, _, a) = pixel(&data, 4, 1, 1);
        assert_eq!(a, 0xFF, "the canvas must stay opaque");
        assert!(
            r > 0x16 && r < 0xFF,
            "half alpha should land between, got {r:#04x}"
        );
    }

    /// Nothing may be written outside the buffer, whatever it is asked to
    /// draw. A wallpaper that panics on an odd screen size is a black desktop.
    #[test]
    fn blending_off_the_edge_is_dropped_rather_than_wrapped() {
        let mut data = buffer(4, 4);
        let mut canvas = Canvas::new(&mut data, 4, 4);
        canvas.fill(BACKDROP);
        for (x, y) in [(-1, 0), (0, -1), (4, 0), (0, 4), (i32::MIN, i32::MAX)] {
            canvas.blend(x, y, ACCENT, 1.0);
        }
        assert!(
            data.chunks_exact(4).all(|p| p == BACKDROP.to_bgra()),
            "an out-of-bounds blend touched a pixel"
        );
    }

    #[test]
    fn setting_a_pixel_writes_it_opaquely_and_clips() {
        let mut data = buffer(4, 4);
        let mut canvas = Canvas::new(&mut data, 4, 4);
        canvas.fill(BACKDROP);
        canvas.set_opaque(1, 2, 0x11, 0x22, 0x33);
        for (x, y) in [(-1, 0), (0, -1), (4, 0), (0, 4)] {
            canvas.set_opaque(x, y, 0xFF, 0xFF, 0xFF);
        }

        assert_eq!(pixel(&data, 4, 1, 2), (0x11, 0x22, 0x33, 0xFF));
        assert_eq!(
            pixel(&data, 4, 0, 0),
            (0x16, 0x16, 0x1F, 0xFF),
            "clipped writes leaked"
        );
    }

    #[test]
    fn a_blit_reproduces_its_source_exactly() {
        let source: Vec<u8> = (0..(8 * 4 * 4)).map(|i| (i % 251) as u8).collect();
        let mut data = buffer(8, 4);
        Canvas::new(&mut data, 8, 4).blit(&source);
        assert_eq!(data, source);
    }

    #[test]
    fn a_blit_of_the_wrong_size_is_refused_rather_than_torn() {
        let mut data = buffer(8, 4);
        Canvas::new(&mut data, 8, 4).fill(BACKDROP);
        let before = data.clone();
        Canvas::new(&mut data, 8, 4).blit(&[0xFF; 16]);
        assert_eq!(data, before, "a short source must leave the canvas alone");
    }

    /// The ends of a fade must be exact. One level off at `t = 1.0` is a
    /// visible step at the moment the transition is torn down and the new
    /// image is blitted straight.
    #[test]
    fn a_crossfade_reproduces_its_ends_byte_for_byte() {
        let a = vec![0x20u8; 4 * 4 * 4];
        let b = vec![0xE0u8; 4 * 4 * 4];
        let mut data = buffer(4, 4);

        Canvas::new(&mut data, 4, 4).crossfade(&a, &b, 0.0);
        assert_eq!(data, a);
        Canvas::new(&mut data, 4, 4).crossfade(&a, &b, 1.0);
        assert_eq!(data, b);
    }

    #[test]
    fn a_crossfade_lands_between_its_ends() {
        let a = vec![0x00u8; 4 * 4 * 4];
        let b = vec![0xFFu8; 4 * 4 * 4];
        let mut data = buffer(4, 4);
        Canvas::new(&mut data, 4, 4).crossfade(&a, &b, 0.5);
        assert!(
            data.iter().all(|&v| (0x7E..=0x81).contains(&v)),
            "got {:#04x}",
            data[0]
        );
    }

    #[test]
    fn a_crossfade_is_clamped_at_both_ends() {
        let a = vec![0x10u8; 4 * 4 * 4];
        let b = vec![0xF0u8; 4 * 4 * 4];
        let mut data = buffer(4, 4);
        Canvas::new(&mut data, 4, 4).crossfade(&a, &b, -3.0);
        assert_eq!(data, a);
        Canvas::new(&mut data, 4, 4).crossfade(&a, &b, 7.0);
        assert_eq!(data, b);
    }

    #[test]
    fn a_scrim_darkens_towards_its_colour_and_stays_opaque() {
        let mut data = buffer(4, 4);
        let mut canvas = Canvas::new(&mut data, 4, 4);
        canvas.fill(Color::rgb(255, 255, 255));
        canvas.scrim(Color::BLACK.with_alpha(0xC0));

        let (r, _, _, a) = pixel(&data, 4, 0, 0);
        assert!(
            r < 0x50,
            "a heavy scrim should darken a white canvas, got {r:#04x}"
        );
        assert_eq!(a, 0xFF);
    }

    #[test]
    fn a_fully_transparent_scrim_changes_nothing() {
        let mut data = buffer(4, 4);
        let mut canvas = Canvas::new(&mut data, 4, 4);
        canvas.fill(ACCENT);
        let before = canvas.as_bytes().to_vec();
        canvas.scrim(Color::BLACK.with_alpha(0));
        assert_eq!(canvas.as_bytes(), &before[..]);
    }

    /// An shm pool is allocated for the largest screen seen and handed out for
    /// smaller ones, so this is the normal case and must not draw past the
    /// live area.
    #[test]
    fn an_oversized_buffer_is_only_written_where_the_canvas_is() {
        let mut data = vec![0xAAu8; 16 * 16 * 4];
        Canvas::new(&mut data, 4, 4).fill(ACCENT);
        assert_eq!(&data[4 * 4 * 4..4 * 4 * 4 + 4], &[0xAA; 4]);
    }

    #[test]
    fn a_zero_sized_canvas_draws_nothing_and_does_not_panic() {
        let mut data = buffer(1, 1);
        let mut canvas = Canvas::new(&mut data, 0, 0);
        canvas.fill(ACCENT);
        canvas.gradient(ACCENT, BACKDROP);
        canvas.blend(0, 0, ACCENT, 1.0);
        canvas.blit(&[]);
        canvas.crossfade(&[], &[], 0.5);
        canvas.scrim(Color::BLACK.with_alpha(0x80));
        assert!(canvas.as_bytes().is_empty());
    }
}
