//! A dark sky, and stars drawn on top of it at full resolution.
//!
//! This is the scene that motivated splitting rendering in two. The sky is a
//! smooth vertical wash with a slow nebula in it -- exactly the kind of thing
//! [`raven_paint::Field`] exists to render cheaply -- but a star is one or two
//! pixels, and a one-or-two-pixel feature drawn into a field and upscaled
//! eightfold is not a star, it is a smudge.
//!
//! So the sky goes through the field and the stars do not. Drawing them at
//! full resolution costs almost nothing because they are *sparse*: a thousand
//! stars with a three-pixel footprint touch nine thousand pixels, against the
//! eight million in a 4K screen.
//!
//! # The palette
//!
//! The **last** stop is the colour of the stars. Everything before it is the
//! sky, top to bottom. A one-colour palette gives a flat sky with white stars.

use std::f32::consts::TAU;

use raven_paint::{Canvas, Color, Field};

use crate::noise;
use crate::palette::Palette;

/// One star per this many screen pixels.
///
/// About 1,300 on a 1080p screen and 5,000 on a 4K one, before the clamp. Star
/// *density* rather than a fixed count, so a large monitor does not look
/// emptier than a small one.
const PIXELS_PER_STAR: u64 = 1_600;

/// Bounds on the star count, whatever the screen size says.
///
/// The ceiling is the real one: it is what stops a hypothetical enormous
/// surface from turning a sparse detail pass into a dense one.
const STAR_LIMITS: std::ops::RangeInclusive<u64> = 120..=6_000;

/// Times a star's brightness cycles per loop. An integer, so the twinkle
/// returns to where it started; see the crate documentation.
const TWINKLE_CYCLES: f32 = 7.0;

/// How far the field drifts sideways over a loop, as a fraction of the screen.
///
/// One whole screen width for the nearest stars, so the drift wraps exactly
/// and there is no seam.
const DRIFT: f32 = 1.0;

pub(crate) fn paint(field: &mut Field, palette: &Palette, phase: f32) {
    let aspect = field.aspect();
    // The sky is every stop but the last. With only one stop there is nothing
    // to take away, so the sky is that colour and the stars fall back to white.
    let sky = sky_palette(palette);
    let drift = (phase * TAU).sin() * 0.8;

    field.paint(|u, v| {
        let x = (u - 0.5) * aspect;
        // A very low-frequency wash, so the sky is not a flat ramp. Two
        // octaves is enough at this amplitude; a third would not be visible.
        let nebula = noise::fbm(x * 1.4 + drift, v * 1.4, 0xC1A0, 2);
        let height = (v + (nebula - 0.5) * 0.35).clamp(0.0, 1.0);
        sky.ramp(height)
    });
}

/// Draw the stars, at the surface's own resolution.
pub(crate) fn detail(canvas: &mut Canvas<'_>, palette: &Palette, phase: f32) {
    let (width, height) = (canvas.width(), canvas.height());
    if width <= 0 || height <= 0 {
        return;
    }

    let area = width as u64 * height as u64;
    let count = (area / PIXELS_PER_STAR).clamp(*STAR_LIMITS.start(), *STAR_LIMITS.end()) as u32;
    let colour = star_colour(palette);
    let (w, h) = (width as f32, height as f32);

    for index in 0..count {
        let [hx, hy, twinkle_phase, depth] = noise::quad(index, 0xA57E);

        // Nearer stars are brighter, bigger and drift further -- the whole of
        // the parallax, from one hashed number.
        let depth = 0.25 + 0.75 * depth;

        // `rem_euclid` on a value that advances by exactly `DRIFT * depth`
        // over a loop: the star leaves one edge and arrives at the other, and
        // is in the same place at phase 1 as at phase 0.
        let x = (hx + phase * DRIFT * depth).rem_euclid(1.0) * w;
        let y = hy * h;

        let twinkle = 0.55 + 0.45 * (phase * TWINKLE_CYCLES * TAU + twinkle_phase * TAU).sin();
        dot(canvas, x, y, 0.35 + 0.9 * depth, colour, depth * twinkle);
    }
}

/// The sky ramp: every stop but the last, or the only stop there is.
fn sky_palette(palette: &Palette) -> Palette {
    let stops = palette.stops();
    let sky = if stops.len() > 1 {
        stops[..stops.len() - 1].to_vec()
    } else {
        stops.to_vec()
    };
    // Cannot fail: `stops` is never empty, so neither is this.
    Palette::new(sky).unwrap_or_else(|_| Palette::solid(Color::BLACK))
}

/// The colour of a star.
fn star_colour(palette: &Palette) -> Color {
    if palette.len() > 1 {
        palette.color(palette.len() - 1)
    } else {
        Color::rgb(0xFF, 0xFF, 0xFF)
    }
}

/// A soft round point, anti-aliased through a distance field.
///
/// The same closed-form coverage RavenLogin's greeter draws its circles with:
/// for a disc the distance to the edge is exact, so anti-aliasing is one
/// `clamp` per pixel rather than four or sixteen samples.
fn dot(canvas: &mut Canvas<'_>, cx: f32, cy: f32, radius: f32, colour: Color, brightness: f32) {
    let brightness = brightness.clamp(0.0, 1.0);
    if brightness <= 0.01 {
        return;
    }
    let radius = radius.max(0.3);

    // One pixel of margin so the anti-aliased edge is not clipped. The bounds
    // may hang off the screen; `Canvas::blend` drops what does.
    let low = |v: f32| (v - radius - 1.0).floor() as i32;
    let high = |v: f32| (v + radius + 1.0).ceil() as i32;

    for y in low(cy)..=high(cy) {
        for x in low(cx)..=high(cx) {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let distance = (dx * dx + dy * dy).sqrt() - radius;
            // The one-pixel linear ramp either side of the boundary is what
            // makes a star look drawn rather than rasterized.
            let coverage = (0.5 - distance).clamp(0.0, 1.0);
            canvas.blend(x, y, colour, coverage * brightness);
        }
    }
}
