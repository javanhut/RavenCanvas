//! Deterministic pseudo-randomness, without a random number generator.
//!
//! Nothing here has state and nothing here reads a clock. Everything is a
//! hash of its arguments, which buys three things a `rand` dependency would
//! not:
//!
//! - **The same frame renders identically every time.** `--preview` output can
//!   be compared against the last one, and a scene's tests can assert on
//!   pixels rather than on statistics.
//! - **Two screens showing the same scene agree.** A starfield on a laptop
//!   panel and on the monitor beside it place their stars in the same relative
//!   positions, because both derive them from the star's index.
//! - **A scene can be sampled anywhere, in any order.** The upscale in
//!   `raven_paint::Field` and the detail pass do not have to visit pixels in
//!   the same sequence to see the same picture.
//!
//! All of this is *spatial*. Nothing here takes time as an input: everything
//! that moves in a scene moves on a sine of the loop phase, so that scenes are
//! exactly periodic. See the crate documentation for why that matters.

/// A 32-bit integer hash.
///
/// The finalizer from `splitmix32`. It is not cryptographic and does not need
/// to be -- the requirement is that adjacent inputs give unrelated outputs, so
/// that a lattice of hashed integer coordinates looks like noise rather than
/// like a lattice.
#[must_use]
pub(crate) fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// A hash of two integer coordinates and a seed.
#[must_use]
pub(crate) fn hash2(x: i32, y: i32, seed: u32) -> u32 {
    hash(x as u32 ^ hash(y as u32 ^ hash(seed)))
}

/// A hash, as a float in `0.0..1.0`.
///
/// The top 24 bits, because that is exactly `f32`'s mantissa: taking all 32
/// and dividing would round, and the two extremes would then be reachable
/// where the others are not.
#[must_use]
pub(crate) fn unit(h: u32) -> f32 {
    (h >> 8) as f32 / 16_777_216.0 // 2^24
}

/// Value noise on a unit lattice, smoothly interpolated, in `0.0..=1.0`.
///
/// Value rather than gradient (Perlin) noise: this is used for the texture in
/// a curtain of light seen across a whole screen, where the difference between
/// the two is not visible, and value noise is half the arithmetic.
#[must_use]
pub(crate) fn value(x: f32, y: f32, seed: u32) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let (ix, iy) = (x0 as i32, y0 as i32);

    // Smoothstep, so the lattice does not show as a grid of creases. Linear
    // interpolation between lattice points is continuous but its *derivative*
    // is not, and the eye finds that edge immediately.
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    let (sx, sy) = (smooth(fx), smooth(fy));

    let at = |dx: i32, dy: i32| unit(hash2(ix + dx, iy + dy, seed));
    let top = at(0, 0) + (at(1, 0) - at(0, 0)) * sx;
    let bottom = at(0, 1) + (at(1, 1) - at(0, 1)) * sx;
    top + (bottom - top) * sy
}

/// Several octaves of [`value`] summed, in `0.0..=1.0`.
///
/// Each octave is twice the frequency and half the amplitude, which is the
/// usual choice and the one that looks like weather. The sum is normalized by
/// the total amplitude rather than by its observed range, so the result is
/// guaranteed inside the unit interval without a clamp hiding a mistake.
#[must_use]
pub(crate) fn fbm(x: f32, y: f32, seed: u32, octaves: u32) -> f32 {
    let mut sum = 0.0;
    let mut amplitude = 1.0;
    let mut total = 0.0;
    let mut frequency = 1.0;

    for octave in 0..octaves.max(1) {
        sum += value(x * frequency, y * frequency, seed.wrapping_add(octave)) * amplitude;
        total += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    sum / total
}

/// Four independent `0.0..1.0` values for the `index`th of something.
///
/// A starfield needs a position, a phase and a depth per star, and taking them
/// from four hashes of the same index is how it gets them without storing
/// anything.
#[must_use]
pub(crate) fn quad(index: u32, seed: u32) -> [f32; 4] {
    let base = hash(index ^ hash(seed));
    [
        unit(base),
        unit(hash(base ^ 0x9E37_79B9)),
        unit(hash(base ^ 0x85EB_CA6B)),
        unit(hash(base ^ 0xC2B2_AE35)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_a_function_of_its_arguments_only() {
        assert_eq!(hash(12345), hash(12345));
        assert_eq!(hash2(3, -4, 7), hash2(3, -4, 7));
        assert_eq!(quad(9, 1), quad(9, 1));
    }

    /// The one property that actually matters: neighbouring lattice points
    /// must be unrelated. A hash that leaves them correlated shows up as
    /// diagonal streaks across the whole screen.
    #[test]
    fn adjacent_coordinates_hash_to_unrelated_values() {
        let mut correlated = 0;
        for i in 0..512 {
            let a = unit(hash2(i, 0, 0));
            let b = unit(hash2(i + 1, 0, 0));
            if (a - b).abs() < 0.02 {
                correlated += 1;
            }
        }
        // Two independent uniforms land within 0.02 about 4% of the time.
        assert!(
            correlated < 50,
            "{correlated}/512 neighbours were near-identical"
        );
    }

    #[test]
    fn unit_values_are_inside_the_unit_range() {
        for i in 0..2000u32 {
            let v = unit(hash(i));
            assert!((0.0..1.0).contains(&v), "hash({i}) gave {v}");
        }
        assert_eq!(unit(0), 0.0);
        assert!(
            unit(u32::MAX) < 1.0,
            "the top of the range must stay exclusive"
        );
    }

    #[test]
    fn unit_values_cover_the_range_evenly() {
        // Ten buckets over two thousand samples: a badly-scaled hash piles up
        // in one of them.
        let mut buckets = [0u32; 10];
        for i in 0..2000u32 {
            buckets[(unit(hash(i)) * 10.0) as usize] += 1;
        }
        for (bucket, count) in buckets.iter().enumerate() {
            assert!(
                (120..280).contains(count),
                "bucket {bucket} had {count} of 2000"
            );
        }
    }

    #[test]
    fn value_noise_stays_inside_the_unit_range() {
        let mut x = -20.0f32;
        while x < 20.0 {
            let v = value(x, x * 0.37, 3);
            assert!((0.0..=1.0).contains(&v), "at {x}: {v}");
            x += 0.019;
        }
    }

    /// Value noise must be continuous. A discontinuity at a lattice boundary
    /// is a visible crease across the screen, and it is the failure mode of
    /// getting the interpolation wrong.
    #[test]
    fn value_noise_is_continuous_across_a_lattice_boundary() {
        let before = value(0.99999, 0.5, 1);
        let after = value(1.00001, 0.5, 1);
        assert!((before - after).abs() < 0.001, "{before} then {after}");
    }

    #[test]
    fn value_noise_actually_varies() {
        let samples: Vec<f32> = (0..64).map(|i| value(i as f32 * 0.7, 0.3, 5)).collect();
        let low = samples.iter().copied().fold(f32::MAX, f32::min);
        let high = samples.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            high - low > 0.4,
            "noise spanning only {} is not noise",
            high - low
        );
    }

    #[test]
    fn fbm_stays_inside_the_unit_range() {
        let mut x = -8.0f32;
        while x < 8.0 {
            let v = fbm(x, x * 0.61, 11, 4);
            assert!((0.0..=1.0).contains(&v), "at {x}: {v}");
            x += 0.031;
        }
    }

    #[test]
    fn fbm_of_one_octave_is_plain_value_noise() {
        assert_eq!(fbm(1.5, 2.5, 4, 1), value(1.5, 2.5, 4));
        // And zero octaves is treated as one rather than dividing by zero.
        assert!(fbm(1.5, 2.5, 4, 0).is_finite());
    }

    #[test]
    fn a_quad_gives_four_unrelated_values() {
        let q = quad(42, 0);
        for v in q {
            assert!((0.0..1.0).contains(&v), "{q:?}");
        }
        // If two entries came from the same hash they would be equal.
        for i in 0..4 {
            for j in (i + 1)..4 {
                assert_ne!(q[i], q[j], "entries {i} and {j} of {q:?}");
            }
        }
    }

    #[test]
    fn consecutive_indices_give_unrelated_quads() {
        let mut clashes = 0;
        for i in 0..500 {
            if (quad(i, 0)[0] - quad(i + 1, 0)[0]).abs() < 0.01 {
                clashes += 1;
            }
        }
        assert!(
            clashes < 30,
            "{clashes}/500 consecutive stars nearly coincided"
        );
    }
}
