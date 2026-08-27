//! A low-resolution buffer that scenes are drawn into and upscaled from.
//!
//! # Why this exists
//!
//! A procedural background is arithmetic per pixel, every frame. At 4K that is
//! 8.3 million pixels; at 30 frames a second it is a quarter of a billion
//! evaluations a second, for a picture nobody is looking at. No amount of
//! tuning the noise function makes that a reasonable thing for a *wallpaper*
//! to do to a laptop.
//!
//! But the scenes this engine ships -- flowing gradients, aurora, plasma --
//! are all *low-frequency*. There is no detail in them finer than tens of
//! pixels, so almost all of that arithmetic is spent computing values that
//! could have been interpolated. Rendering into a field a few hundred pixels
//! across and bilinearly upscaling it is visually indistinguishable and
//! typically **two orders of magnitude cheaper**: a 480x270 field is 130,000
//! evaluations rather than 8.3 million.
//!
//! What upscaling cannot produce is a sharp point, which is why
//! [`raven_scene`](../raven_scene/index.html)'s starfield draws its stars onto
//! the canvas at full resolution *after* its field has been upscaled. Sparse
//! detail is cheap; smooth fields are not. Splitting a scene along that line
//! is the whole trick.
//!
//! # Colour
//!
//! Channels are `f32` in `0.0..=1.0`. Scenes are written in floating point
//! because that is the arithmetic they are, and quantizing once -- on the way
//! out, with a dither -- is what stops a shallow field from banding when it is
//! stretched across a screen.

use crate::canvas::Canvas;
use crate::dither;

/// A small RGB buffer, in floating point.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    width: usize,
    height: usize,
    /// `width * height * 3` channels, `0.0..=1.0`, row-major.
    pixels: Vec<f32>,
}

impl Field {
    /// An all-black field of exactly this size.
    ///
    /// Both dimensions are floored at 1: a zero-sized field would make every
    /// sample below a division by zero, and there is no size at which "no
    /// pixels" is a more useful answer than "one".
    #[must_use]
    pub fn new(width: usize, height: usize) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        Self {
            width,
            height,
            pixels: vec![0.0; width * height * 3],
        }
    }

    /// The field to render a `width` x `height` surface with, at most
    /// `max_edge` pixels on its longer axis.
    ///
    /// The surface's aspect ratio is preserved, so the upscale is uniform and
    /// a circle in a scene stays a circle. A surface already smaller than
    /// `max_edge` is rendered at its own size rather than upscaled from
    /// something smaller -- there is nothing to be saved, and it would be
    /// softer for no reason.
    #[must_use]
    pub fn for_surface(width: i32, height: i32, max_edge: u32) -> Self {
        let (w, h) = (width.max(1) as f32, height.max(1) as f32);
        let scale = (max_edge.clamp(1, 8192) as f32 / w.max(h)).min(1.0);
        Self::new((w * scale).round() as usize, (h * scale).round() as usize)
    }

    /// Resize in place if `width` x `height` at `max_edge` asks for a
    /// different size, and say whether anything changed.
    ///
    /// The engine calls this every frame and it almost always does nothing,
    /// which is the point: reallocating a scene buffer per frame would be the
    /// only allocation in the render loop.
    pub fn resize_for(&mut self, width: i32, height: i32, max_edge: u32) -> bool {
        let wanted = Self::for_surface(width, height, max_edge);
        if wanted.width == self.width && wanted.height == self.height {
            return false;
        }
        *self = wanted;
        true
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Width over height. Scenes multiply their horizontal coordinate by this
    /// so their shapes are not stretched by the screen's aspect ratio.
    #[must_use]
    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height as f32
    }

    /// Fill every pixel from a function of its normalized position.
    ///
    /// `paint` is called with `(u, v)` at the pixel's centre, each in
    /// `0.0..1.0`, and returns linear-ish RGB in `0.0..=1.0`. Values outside
    /// that range are clamped on the way out rather than here, so a scene may
    /// overshoot while it accumulates.
    ///
    /// This is the hot loop of the whole engine. It is a plain nested `for`
    /// with the closure inlined into it, and it is deliberately not
    /// parallelized: a wallpaper that saturates every core to draw itself has
    /// misunderstood what it is for.
    pub fn paint<F>(&mut self, mut paint: F)
    where
        F: FnMut(f32, f32) -> [f32; 3],
    {
        let (w, h) = (self.width as f32, self.height as f32);
        for y in 0..self.height {
            let v = (y as f32 + 0.5) / h;
            let row = y * self.width * 3;
            for x in 0..self.width {
                let u = (x as f32 + 0.5) / w;
                let rgb = paint(u, v);
                let i = row + x * 3;
                self.pixels[i] = rgb[0];
                self.pixels[i + 1] = rgb[1];
                self.pixels[i + 2] = rgb[2];
            }
        }
    }

    /// The RGB at a field pixel, clamped to the field's bounds.
    #[must_use]
    pub fn get(&self, x: usize, y: usize) -> [f32; 3] {
        let x = x.min(self.width - 1);
        let y = y.min(self.height - 1);
        let i = (y * self.width + x) * 3;
        [self.pixels[i], self.pixels[i + 1], self.pixels[i + 2]]
    }

    /// Stretch this field across the whole canvas, bilinearly, dithered.
    ///
    /// Bilinear rather than nearest for the obvious reason -- a 480-wide field
    /// on a 3840-wide screen is an eightfold upscale, and nearest-neighbour
    /// would put visible 8-pixel blocks on the desktop. Dithered because the
    /// interpolated values in between land fractions of a level apart, and
    /// truncating those is exactly the banding [`crate::dither`] describes.
    pub fn upscale_into(&self, canvas: &mut Canvas<'_>) {
        let (dst_w, dst_h) = (canvas.width(), canvas.height());
        if dst_w <= 0 || dst_h <= 0 {
            return;
        }

        let scale_x = self.width as f32 / dst_w as f32;
        let scale_y = self.height as f32 / dst_h as f32;
        let max_x = self.width - 1;
        let max_y = self.height - 1;

        for y in 0..dst_h {
            // Pixel centre to pixel centre, then half a texel back so `y0` is
            // the sample above the point rather than the one containing it.
            let fy = ((y as f32 + 0.5) * scale_y - 0.5).clamp(0.0, max_y as f32);
            let y0 = fy.floor();
            let ty = fy - y0;
            let (y0, y1) = (y0 as usize, (y0 as usize + 1).min(max_y));

            for x in 0..dst_w {
                let fx = ((x as f32 + 0.5) * scale_x - 0.5).clamp(0.0, max_x as f32);
                let x0 = fx.floor();
                let tx = fx - x0;
                let (x0, x1) = (x0 as usize, (x0 as usize + 1).min(max_x));

                let (a, b) = (self.get(x0, y0), self.get(x1, y0));
                let (c, d) = (self.get(x0, y1), self.get(x1, y1));

                let mut rgb = [0.0f32; 3];
                for (channel, out) in rgb.iter_mut().enumerate() {
                    let top = a[channel] + (b[channel] - a[channel]) * tx;
                    let bottom = c[channel] + (d[channel] - c[channel]) * tx;
                    *out = (top + (bottom - top) * ty) * 255.0;
                }

                // Blend at full coverage would go through `Canvas::blend` and
                // its per-pixel float alpha; this is the ground, so it is
                // written straight.
                canvas.set_opaque(
                    x,
                    y,
                    dither::quantize(rgb[0], x, y),
                    dither::quantize(rgb[1], x, y),
                    dither::quantize(rgb[2], x, y),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(data: &[u8], width: i32, x: i32, y: i32) -> (u8, u8, u8, u8) {
        let i = ((y * width + x) * 4) as usize;
        (data[i + 2], data[i + 1], data[i], data[i + 3])
    }

    #[test]
    fn a_field_never_has_zero_pixels() {
        let field = Field::new(0, 0);
        assert_eq!((field.width(), field.height()), (1, 1));
    }

    #[test]
    fn sizing_preserves_the_aspect_ratio_and_caps_the_long_edge() {
        let field = Field::for_surface(3840, 2160, 480);
        assert_eq!((field.width(), field.height()), (480, 270));
        assert!((field.aspect() - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn sizing_caps_the_long_edge_whichever_axis_it_is() {
        let portrait = Field::for_surface(1080, 1920, 480);
        assert_eq!((portrait.width(), portrait.height()), (270, 480));
    }

    /// Rendering a small screen through a smaller field would be softer for
    /// nothing: there is no work to save.
    #[test]
    fn a_surface_smaller_than_the_cap_is_rendered_at_its_own_size() {
        let field = Field::for_surface(320, 200, 480);
        assert_eq!((field.width(), field.height()), (320, 200));
    }

    #[test]
    fn resizing_reports_whether_it_did_anything() {
        let mut field = Field::for_surface(1920, 1080, 480);
        assert!(
            !field.resize_for(1920, 1080, 480),
            "the same size must not reallocate"
        );
        assert!(field.resize_for(1080, 1920, 480), "a rotation must");
        assert_eq!((field.width(), field.height()), (270, 480));
    }

    #[test]
    fn painting_visits_every_pixel_at_its_centre() {
        let mut field = Field::new(4, 2);
        let mut seen = Vec::new();
        field.paint(|u, v| {
            seen.push((u, v));
            [u, v, 0.0]
        });
        assert_eq!(seen.len(), 8);
        assert_eq!(seen[0], (0.125, 0.25), "the first pixel's centre");
        assert_eq!(seen[7], (0.875, 0.75), "the last pixel's centre");
        assert_eq!(field.get(0, 0), [0.125, 0.25, 0.0]);
    }

    #[test]
    fn getting_off_the_edge_clamps_rather_than_panicking() {
        let mut field = Field::new(2, 2);
        field.paint(|u, _| [u, 0.0, 0.0]);
        assert_eq!(field.get(99, 99), field.get(1, 1));
    }

    /// The upscale must reach both ends of the field. A common off-by-half
    /// here leaves the last row of a screen a shade short of the field's last
    /// row, which shows up as a hairline along the bottom edge.
    #[test]
    fn an_upscale_reaches_both_ends_of_the_field() {
        let mut field = Field::new(2, 1);
        field.paint(|u, _| {
            if u < 0.5 {
                [0.0, 0.0, 0.0]
            } else {
                [1.0, 1.0, 1.0]
            }
        });

        let mut data = vec![0u8; 32 * 4 * 4];
        field.upscale_into(&mut Canvas::new(&mut data, 32, 4));

        assert!(
            pixel(&data, 32, 0, 0).0 < 4,
            "the left edge should be the field's black"
        );
        assert!(
            pixel(&data, 32, 31, 3).0 > 251,
            "the right edge should be the field's white"
        );
    }

    #[test]
    fn an_upscale_interpolates_rather_than_blocking() {
        let mut field = Field::new(2, 1);
        field.paint(|u, _| {
            if u < 0.5 {
                [0.0, 0.0, 0.0]
            } else {
                [1.0, 1.0, 1.0]
            }
        });

        let mut data = vec![0u8; 32 * 1 * 4];
        field.upscale_into(&mut Canvas::new(&mut data, 32, 1));

        let row: Vec<u8> = (0..32).map(|x| pixel(&data, 32, x, 0).0).collect();
        let distinct: std::collections::BTreeSet<u8> = row.iter().copied().collect();
        assert!(
            distinct.len() > 8,
            "a bilinear upscale should be a ramp, not two blocks; saw {} levels",
            distinct.len()
        );
        assert!(
            row.windows(2).all(|w| w[0] <= w[1].saturating_add(1)),
            "and a monotonic one: {row:?}"
        );
    }

    #[test]
    fn an_upscale_is_opaque_everywhere() {
        let mut field = Field::new(3, 3);
        field.paint(|u, v| [u, v, 0.5]);
        let mut data = vec![0u8; 17 * 13 * 4];
        field.upscale_into(&mut Canvas::new(&mut data, 17, 13));
        assert!(data.chunks_exact(4).all(|p| p[3] == 0xFF));
    }

    /// A flat field must come out flat to within the dither, and no more. The
    /// failure this guards against is structure the *upscale* invented -- a
    /// seam, a block edge, a gradient across a constant region -- which would
    /// show as pixels several levels apart rather than one.
    #[test]
    fn a_flat_field_upscales_to_a_flat_screen() {
        let mut field = Field::new(8, 8);
        field.paint(|_, _| [0.25, 0.5, 0.75]);
        let mut data = vec![0u8; 40 * 40 * 4];
        field.upscale_into(&mut Canvas::new(&mut data, 40, 40));

        for channel in 0..3 {
            let values: Vec<u8> = data.chunks_exact(4).map(|p| p[channel]).collect();
            let (low, high) = (
                *values.iter().min().expect("a screen has pixels"),
                *values.iter().max().expect("a screen has pixels"),
            );
            assert!(
                high - low <= 1,
                "a constant field varied by {} levels on channel {channel}",
                high - low
            );
        }
    }

    #[test]
    fn values_outside_the_unit_range_are_clamped_rather_than_wrapped() {
        let mut field = Field::new(2, 2);
        field.paint(|u, _| {
            if u < 0.5 {
                [-3.0, -3.0, -3.0]
            } else {
                [4.0, 4.0, 4.0]
            }
        });
        let mut data = vec![0u8; 8 * 8 * 4];
        field.upscale_into(&mut Canvas::new(&mut data, 8, 8));
        assert_eq!(pixel(&data, 8, 0, 0).0, 0);
        assert_eq!(pixel(&data, 8, 7, 0).0, 255);
    }

    #[test]
    fn upscaling_into_a_zero_sized_canvas_does_nothing_and_does_not_panic() {
        let field = Field::new(4, 4);
        let mut data = vec![0u8; 4];
        field.upscale_into(&mut Canvas::new(&mut data, 0, 0));
    }
}
