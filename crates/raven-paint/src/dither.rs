//! An ordered dither, shared by the two places that quantize to eight bits.
//!
//! Both a shallow gradient and an upscaled [`crate::Field`] have the same
//! problem: the value they want to write changes by less than one level over
//! many pixels, so rounding to the nearest byte produces flat bands with hard
//! edges between them. On a large dark panel those edges are clearly visible.
//!
//! Perturbing the value by a threshold that varies across a small tile, before
//! truncating, trades the hard edges for a stipple far below the noise floor
//! of any real panel.
//!
//! Ordered rather than random because it is **deterministic**: the same frame
//! renders identically every time, so `--preview` output can be compared
//! against the last one, and a scene's tests can assert on exact bytes.

/// A 4x4 Bayer matrix, as thresholds in `0.0..1.0`.
///
/// The entries are `(2n + 1) / 32` for the classic Bayer ordering `n`, which
/// puts them at the *centres* of sixteen equal slices of the interval rather
/// than at `n / 16`. That detail is the difference between an unbiased dither
/// and one that darkens the whole image by half a level: [`quantize`]
/// truncates, so it subtracts on average exactly what these add, and only if
/// their mean is 0.5.
#[rustfmt::skip]
const BAYER: [f32; 16] = [
     1.0 / 32.0, 17.0 / 32.0,  5.0 / 32.0, 21.0 / 32.0,
    25.0 / 32.0,  9.0 / 32.0, 29.0 / 32.0, 13.0 / 32.0,
     7.0 / 32.0, 23.0 / 32.0,  3.0 / 32.0, 19.0 / 32.0,
    31.0 / 32.0, 15.0 / 32.0, 27.0 / 32.0, 11.0 / 32.0,
];

/// The threshold for one pixel, in `0.0..1.0`.
#[must_use]
pub(crate) fn offset(x: i32, y: i32) -> f32 {
    BAYER[((y & 3) * 4 + (x & 3)) as usize]
}

/// Quantize a `0.0..=255.0` channel to a byte, dithered at `(x, y)`.
///
/// Truncating, not rounding: adding a threshold uniform over `0.0..1.0` and
/// truncating *is* rounding, with the rounding decision spread across the tile
/// instead of taken the same way at every pixel. That is the whole mechanism.
#[must_use]
pub(crate) fn quantize(value: f32, x: i32, y: i32) -> u8 {
    (value + offset(x, y)).clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mean threshold must be exactly half a level, because truncation
    /// takes half a level back. A matrix whose mean is anything else shifts
    /// every dithered pixel on the screen in one direction.
    #[test]
    fn the_matrix_averages_to_half_a_level() {
        let sum: f32 = (0..4).flat_map(|y| (0..4).map(move |x| offset(x, y))).sum();
        assert!(
            (sum - 8.0).abs() < 1e-6,
            "a biased dither shifts the image, sum was {sum}"
        );
    }

    #[test]
    fn every_threshold_is_inside_one_level() {
        for y in 0..4 {
            for x in 0..4 {
                assert!((0.0..1.0).contains(&offset(x, y)), "at {x},{y}");
            }
        }
    }

    /// A value already on a level boundary must survive untouched, or a flat
    /// region of a scene would shimmer for no reason.
    #[test]
    fn an_exact_value_is_not_perturbed() {
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(quantize(37.0, x, y), 37, "at {x},{y}");
            }
        }
    }

    #[test]
    fn the_tile_repeats_every_four_pixels_including_negatives() {
        for y in -8..8 {
            for x in -8..8 {
                assert_eq!(offset(x, y), offset(x + 4, y), "at {x},{y}");
                assert_eq!(offset(x, y), offset(x, y + 4), "at {x},{y}");
            }
        }
    }

    /// The point of the whole file: a value that is not on a level boundary
    /// must come out as a mixture of the two neighbouring bytes rather than as
    /// one flat one.
    #[test]
    fn a_fractional_value_stipples_between_two_levels() {
        let seen: Vec<u8> = (0..4).map(|x| quantize(10.5, x, 0)).collect();
        assert!(seen.contains(&10) && seen.contains(&11), "got {seen:?}");

        // And over the whole tile it averages back to the value asked for.
        let total: u32 = (0..4)
            .flat_map(|y| (0..4).map(move |x| u32::from(quantize(10.5, x, y))))
            .sum();
        assert_eq!(
            total,
            16 * 10 + 8,
            "10.5 over sixteen pixels should total 168"
        );
    }

    #[test]
    fn quantizing_is_clamped_at_both_ends() {
        assert_eq!(quantize(-100.0, 0, 0), 0);
        assert_eq!(quantize(400.0, 0, 0), 255);
        // And at the ends of the legal range, where the dither would otherwise
        // push a value off the end.
        assert_eq!(quantize(255.0, 3, 0), 255);
        assert_eq!(quantize(0.0, 3, 3), 0);
    }
}
