//! A gradient that drifts, and rotates as it drifts.
//!
//! The cheapest scene, and the one that is on by default. Every pixel is one
//! dot product and one palette sample -- no noise, no trigonometry inside the
//! loop -- which at the field resolutions [`raven_paint::Field`] uses is a few
//! tens of thousands of operations a frame. It is the scene to reach for on a
//! machine where the wallpaper must cost nothing measurable.
//!
//! Two things move, and they move at different rates so the pair does not
//! obviously repeat: the palette slides along the axis, and the axis itself
//! turns. Both are driven by the same phase, so both wrap together.

use std::f32::consts::TAU;

use raven_paint::Field;

use crate::palette::Palette;

/// Turns of the gradient's axis per loop.
///
/// One. The axis has to come back to where it started for the loop to be
/// seamless, and more than one turn in ten minutes reads as the picture
/// spinning rather than as light moving.
const TURNS_PER_LOOP: f32 = 1.0;

/// Times the palette slides past a point per loop.
///
/// Three against the axis's one, so the two cycles only coincide once a loop
/// rather than beating against each other.
const SLIDES_PER_LOOP: f32 = 3.0;

/// How much of the palette is visible across the screen at once.
///
/// Less than one: showing the whole ramp at every moment leaves nowhere for it
/// to slide *to*. Two thirds keeps a colour off-screen to arrive.
const SPREAD: f32 = 0.66;

pub(crate) fn paint(field: &mut Field, palette: &Palette, phase: f32) {
    let angle = phase * TURNS_PER_LOOP * TAU;
    let (sin, cos) = angle.sin_cos();
    let aspect = field.aspect();
    let slide = phase * SLIDES_PER_LOOP;

    field.paint(|u, v| {
        // Centred and aspect-corrected, so the axis sweeps evenly rather than
        // being squashed on a wide screen.
        let x = (u - 0.5) * aspect;
        let y = v - 0.5;
        // The projection onto the axis, normalised to roughly `0.0..=1.0`
        // across the diagonal.
        let along = (x * cos + y * sin) / aspect.max(1.0) + 0.5;
        palette.sample(along * SPREAD + slide)
    });
}
