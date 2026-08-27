//! The colours a scene is drawn from.
//!
//! A palette is a **cyclic** ramp: sampling past the last stop wraps back to
//! the first. That is not a convenience, it is what lets a scene animate
//! forever without a seam -- an animation that walks a palette and wraps at
//! `1.0` returns to exactly the colour it started at, so there is no frame
//! where the screen jumps.

use std::fmt;

use raven_paint::Color;

/// One or more colour stops, sampled cyclically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette(Vec<Color>);

impl Palette {
    /// Build a palette from its stops.
    ///
    /// An empty palette is refused rather than filled in with a default:
    /// somebody wrote `palette = []` in a config file and meant something by
    /// it, and quietly drawing them the stock colours would look like the file
    /// was ignored -- which, in the way that matters, it would have been.
    pub fn new(stops: Vec<Color>) -> Result<Self, EmptyPalette> {
        if stops.is_empty() {
            return Err(EmptyPalette);
        }
        Ok(Self(stops))
    }

    /// A palette of one colour.
    #[must_use]
    pub fn solid(color: Color) -> Self {
        Self(vec![color])
    }

    #[must_use]
    pub fn stops(&self) -> &[Color] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        // Never true -- the constructor refuses an empty palette -- but clippy
        // asks for it wherever there is a `len`, and a caller should not have
        // to know that this is a constant.
        false
    }

    /// The stop at `index`, wrapping.
    #[must_use]
    pub fn color(&self, index: usize) -> Color {
        self.0[index % self.0.len()]
    }

    /// Sample the ramp at `t`, wrapping, as RGB in `0.0..=1.0`.
    ///
    /// Linear between adjacent stops. `t` is taken modulo 1, so a scene may
    /// hand this an ever-increasing phase without doing its own wrapping --
    /// and a negative one, which is what a reversed animation produces.
    #[must_use]
    pub fn sample(&self, t: f32) -> [f32; 3] {
        let count = self.0.len();
        if count == 1 {
            return to_rgb(self.0[0]);
        }

        let position = t.rem_euclid(1.0) * count as f32;
        // `position` is in `0.0..count`, but a `t` a hair under 1.0 can round
        // up to exactly `count` in f32. Clamping the index is cheaper than
        // reasoning about which values do.
        let index = (position as usize).min(count - 1);
        let fraction = position - index as f32;

        let a = to_rgb(self.0[index]);
        let b = to_rgb(self.0[(index + 1) % count]);
        [
            a[0] + (b[0] - a[0]) * fraction,
            a[1] + (b[1] - a[1]) * fraction,
            a[2] + (b[2] - a[2]) * fraction,
        ]
    }

    /// Sample without wrapping: `t` is clamped to the ends instead.
    ///
    /// For a scene whose palette is a *range* rather than a loop -- a
    /// starfield's sky runs from its darkest stop to its lightest and does not
    /// come back round.
    #[must_use]
    pub fn ramp(&self, t: f32) -> [f32; 3] {
        let count = self.0.len();
        if count == 1 {
            return to_rgb(self.0[0]);
        }

        let position = t.clamp(0.0, 1.0) * (count - 1) as f32;
        let index = (position as usize).min(count - 2);
        let fraction = position - index as f32;

        let a = to_rgb(self.0[index]);
        let b = to_rgb(self.0[index + 1]);
        [
            a[0] + (b[0] - a[0]) * fraction,
            a[1] + (b[1] - a[1]) * fraction,
            a[2] + (b[2] - a[2]) * fraction,
        ]
    }
}

/// A colour as three `0.0..=1.0` channels. Alpha is dropped: a scene paints
/// the ground, and there is nothing behind it.
fn to_rgb(color: Color) -> [f32; 3] {
    [
        f32::from(color.red()) / 255.0,
        f32::from(color.green()) / 255.0,
        f32::from(color.blue()) / 255.0,
    ]
}

/// A palette with no colours in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyPalette;

impl fmt::Display for EmptyPalette {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a palette needs at least one colour")
    }
}

impl std::error::Error for EmptyPalette {}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: Color = Color::from_argb(0xFF00_0000);
    const WHITE: Color = Color::from_argb(0xFFFF_FFFF);
    const RED: Color = Color::from_argb(0xFFFF_0000);

    fn two() -> Palette {
        Palette::new(vec![BLACK, WHITE]).unwrap()
    }

    #[test]
    fn an_empty_palette_is_refused() {
        assert_eq!(Palette::new(Vec::new()), Err(EmptyPalette));
    }

    #[test]
    fn a_single_stop_is_the_same_colour_everywhere() {
        let solid = Palette::solid(RED);
        for t in [-2.0, 0.0, 0.5, 1.0, 7.3] {
            assert_eq!(solid.sample(t), [1.0, 0.0, 0.0], "at {t}");
            assert_eq!(solid.ramp(t), [1.0, 0.0, 0.0], "at {t}");
        }
    }

    #[test]
    fn sampling_starts_at_the_first_stop() {
        assert_eq!(two().sample(0.0), [0.0, 0.0, 0.0]);
    }

    /// The whole reason the ramp is cyclic: a scene that walks its palette and
    /// wraps must land back where it started, exactly, or the screen jumps
    /// once per loop.
    #[test]
    fn sampling_is_seamless_across_the_wrap() {
        let palette = Palette::new(vec![BLACK, RED, WHITE]).unwrap();
        assert_eq!(palette.sample(0.0), palette.sample(1.0));
        assert_eq!(palette.sample(0.25), palette.sample(3.25));

        // And approaching the wrap from below gets close to the start rather
        // than diverging from it.
        let just_under = palette.sample(0.999);
        let start = palette.sample(0.0);
        for c in 0..3 {
            assert!(
                (just_under[c] - start[c]).abs() < 0.02,
                "{just_under:?} vs {start:?}"
            );
        }
    }

    #[test]
    fn sampling_a_negative_phase_wraps_rather_than_clamping() {
        assert_eq!(two().sample(-0.75), two().sample(0.25));
    }

    #[test]
    fn sampling_interpolates_between_stops() {
        // Two stops means each occupies half the ramp; a quarter of the way
        // through is halfway between black and white.
        let mid = two().sample(0.25);
        assert!((mid[0] - 0.5).abs() < 0.01, "got {mid:?}");
    }

    /// A ramp is a range, not a loop: it must reach the last stop at 1.0 and
    /// never fold back to the first.
    #[test]
    fn a_ramp_runs_from_the_first_stop_to_the_last() {
        let palette = Palette::new(vec![BLACK, RED, WHITE]).unwrap();
        assert_eq!(palette.ramp(0.0), [0.0, 0.0, 0.0]);
        assert_eq!(palette.ramp(1.0), [1.0, 1.0, 1.0]);
        assert_eq!(palette.ramp(0.5), [1.0, 0.0, 0.0], "the middle stop");
    }

    #[test]
    fn a_ramp_clamps_rather_than_wrapping() {
        assert_eq!(two().ramp(-3.0), two().ramp(0.0));
        assert_eq!(two().ramp(9.0), two().ramp(1.0));
    }

    /// Every sample of any palette must be inside the unit range, whatever it
    /// is asked for. A channel outside it would be clamped on the way to the
    /// screen and show as a flat blown-out region.
    #[test]
    fn every_sample_is_inside_the_unit_range() {
        let palette = Palette::new(vec![BLACK, RED, WHITE, RED]).unwrap();
        let mut t = -3.0f32;
        while t < 4.0 {
            for channel in palette.sample(t).into_iter().chain(palette.ramp(t)) {
                assert!((0.0..=1.0).contains(&channel), "at {t}: {channel}");
            }
            t += 0.013;
        }
    }

    #[test]
    fn indexing_wraps() {
        assert_eq!(two().color(0), BLACK);
        assert_eq!(two().color(2), BLACK);
        assert_eq!(two().color(3), WHITE);
    }
}
