//! Sums of sines, mapped through the palette.
//!
//! The oldest trick in the demoscene and still the one with the best ratio of
//! arithmetic to apparent complexity: four sine waves at different angles and
//! frequencies, added, and the total used as a position in a cyclic ramp.
//!
//! Every frequency below is an **integer** multiple of the loop, which is what
//! makes the animation exactly periodic -- see the crate documentation. Change
//! one to a non-integer and the scene will jump once every loop.

use std::f32::consts::TAU;

use raven_paint::Field;

use crate::palette::Palette;

/// The four waves: `(x weight, y weight, spatial frequency, cycles per loop)`.
///
/// The spatial frequencies are deliberately not harmonics of each other --
/// 3, 4.7, 5.3, 7 -- because waves at related frequencies produce a visible
/// grid where their peaks line up. The temporal ones are integers because they
/// have to be.
#[rustfmt::skip]
const WAVES: [(f32, f32, f32, f32); 4] = [
    ( 1.0,  0.0, 3.0, 1.0),
    ( 0.0,  1.0, 4.7, 2.0),
    ( 0.7,  0.7, 5.3, 3.0),
    ( 0.6, -0.8, 7.0, 5.0),
];

pub(crate) fn paint(field: &mut Field, palette: &Palette, phase: f32) {
    let aspect = field.aspect();
    // The temporal offsets are the same for every pixel, so they are computed
    // once rather than four times per pixel.
    let offsets: [f32; 4] = std::array::from_fn(|i| phase * WAVES[i].3 * TAU);

    field.paint(|u, v| {
        let x = (u - 0.5) * aspect;
        let y = v - 0.5;

        let mut total = 0.0;
        for (index, (wx, wy, frequency, _)) in WAVES.iter().enumerate() {
            total += ((x * wx + y * wy) * frequency * TAU + offsets[index]).sin();
        }

        // Four sines sum to `-4.0..=4.0`; map that onto one trip round the
        // palette. Sampling wraps, so nothing has to be clamped.
        palette.sample(total / 8.0 + 0.5)
    });
}
