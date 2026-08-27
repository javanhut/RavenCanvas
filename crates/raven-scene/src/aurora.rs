//! Curtains of light over a dark sky.
//!
//! Three ribbons, each a wandering horizontal line with a soft falloff either
//! side of it, added together over a vertical sky ramp and textured with a
//! little noise so the light has grain rather than being a clean airbrush.
//!
//! This is the most expensive scene in the crate -- it is the only one that
//! evaluates noise per pixel -- which is why it renders into a smaller field
//! than the others. See [`FIELD_EDGE`]. Even so it is well under a tenth of
//! what the same picture would cost at full resolution.

use std::f32::consts::TAU;

use raven_paint::Field;

use crate::noise;
use crate::palette::Palette;

/// The long edge of the field this scene renders into.
///
/// Smaller than the crate default because this is the one scene with real
/// arithmetic in its inner loop. An aurora has nothing in it sharper than a
/// soft-edged ribbon tens of pixels across, so 360 across a 4K screen is a
/// tenfold upscale of something that has no detail to lose.
pub(crate) const FIELD_EDGE: u32 = 360;

/// `(height, thickness, brightness)` per ribbon, top to bottom.
const RIBBONS: [(f32, f32, f32); 3] = [(0.30, 0.10, 1.00), (0.45, 0.16, 0.65), (0.62, 0.22, 0.35)];

/// `(spatial frequency, cycles per loop, amplitude)` for the two waves that
/// make each ribbon wander. Both temporal rates are integers, so the whole
/// scene is exactly periodic; see the crate documentation.
const WANDER: [(f32, f32, f32); 2] = [(1.0, 1.0, 0.055), (2.3, 2.0, 0.030)];

pub(crate) fn paint(field: &mut Field, palette: &Palette, phase: f32) {
    let aspect = field.aspect();
    let stops = palette.len();

    // The sky is the first stop; the ribbons take the ones after it, so a
    // two-colour palette is a single-colour aurora and a five-colour one has
    // four differently tinted curtains.
    let sky = palette.color(0);
    let sky_rgb = [
        f32::from(sky.red()) / 255.0,
        f32::from(sky.green()) / 255.0,
        f32::from(sky.blue()) / 255.0,
    ];

    // Everything constant across the frame, computed once rather than per
    // pixel. The drift is a sine of the phase rather than the phase itself,
    // because the noise field has to come back to where it started.
    let drift = (phase * TAU).sin() * 1.5;
    let angles: [f32; 2] = std::array::from_fn(|i| phase * WANDER[i].1 * TAU);

    field.paint(|u, v| {
        let x = (u - 0.5) * aspect;

        // One noise lookup, shared by every ribbon. Stretched horizontally,
        // because a curtain's grain runs down it rather than across.
        let grain = noise::fbm(x * 3.0, v * 1.5 + drift, 0x5EED, 2);

        let mut rgb = sky_rgb;
        for (index, &(height, thickness, brightness)) in RIBBONS.iter().enumerate() {
            let mut centre = height;
            for (wave, &(frequency, _, amplitude)) in WANDER.iter().enumerate() {
                centre += (x * frequency * TAU + angles[wave] + index as f32).sin() * amplitude;
            }

            // A quartic falloff rather than a Gaussian: it has the same soft
            // shoulder and sharp core without an `exp` in the inner loop, and
            // it reaches zero at a finite distance instead of asymptotically.
            let distance = (v - centre) / thickness;
            let falloff = 1.0 / (1.0 + distance * distance * distance * distance);

            // Curtains hang: bright at the top edge, fading down. Without this
            // a ribbon is a symmetrical bar and reads as a stripe.
            let hang = (1.0 - (v - centre + thickness) / (thickness * 3.0)).clamp(0.0, 1.0);

            let intensity = falloff * brightness * (0.55 + 0.45 * grain) * (0.4 + 0.6 * hang);
            // A ribbon per stop after the sky, wrapping if there are fewer
            // stops than ribbons.
            let colour = palette.color(1 + index % (stops - 1).max(1));
            rgb[0] += f32::from(colour.red()) / 255.0 * intensity;
            rgb[1] += f32::from(colour.green()) / 255.0 * intensity;
            rgb[2] += f32::from(colour.blue()) / 255.0 * intensity;
        }

        // Added light overshoots by design -- three overlapping curtains
        // should be brighter than one. Clamping here rather than letting the
        // field do it keeps the colour, because clamping each channel
        // separately at the far end would shift a blown-out blue towards
        // white.
        let peak = rgb[0].max(rgb[1]).max(rgb[2]);
        if peak > 1.0 {
            let scale = 1.0 / peak;
            rgb[0] *= scale;
            rgb[1] *= scale;
            rgb[2] *= scale;
        }
        rgb
    });
}
