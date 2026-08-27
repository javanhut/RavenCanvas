//! Backgrounds that are computed rather than decoded.
//!
//! Four scenes, all drawn on the CPU, all cheap enough to leave running on a
//! laptop. They exist because a wallpaper that moves is a thing people want
//! and because the alternatives -- a video decoder, or a GPU context and a
//! shader -- would put a great deal of C in a process that is resident for the
//! whole life of a session, to draw something nobody is looking at directly.
//!
//! # The two-part render
//!
//! Every scene draws in up to two passes, and the split is the reason the
//! whole thing is affordable:
//!
//! 1. **The field.** A smooth, low-frequency image rendered into a buffer a
//!    few hundred pixels across and bilinearly upscaled to the screen. Almost
//!    everything is here. See [`raven_paint::Field`] for the arithmetic this
//!    saves -- on a 4K screen it is roughly two orders of magnitude.
//! 2. **The detail.** Sparse, sharp features drawn at the screen's own
//!    resolution, on top. Only [`Kind::Starfield`] has one, and it is what the
//!    split exists for: a star upscaled from a small field is a smudge.
//!
//! # The clock is a phase, not a time
//!
//! Scenes are never handed elapsed seconds. They are handed a **phase** in
//! `0.0..1.0` -- where they are in a loop of [`LOOP_SECONDS`] -- and every
//! temporal frequency in every scene is an *integer* number of cycles per
//! loop. Two things follow, and both matter:
//!
//! - **The animation is exactly periodic.** Phase 1.0 draws the same picture
//!   as phase 0.0, so there is no frame where the wallpaper jumps.
//! - **It stays smooth forever.** A scene driven by seconds-as-`f32` gets
//!   coarser as the session gets older: after a day `f32` cannot represent
//!   steps smaller than about 8 milliseconds, and after a fortnight it cannot
//!   represent a frame at all, so the animation visibly stutters and then
//!   stops. A phase in `0.0..1.0` has the same resolution on day three
//!   hundred as on day one.
//!
//! The reduction to a phase is done in `f64` -- see [`Scene::phase`] -- so the
//! precision is only spent once, on the way in.

#![forbid(unsafe_code)]

mod aurora;
mod gradient;
mod noise;
mod palette;
mod plasma;
mod starfield;

use std::fmt;
use std::str::FromStr;

use raven_paint::{Canvas, Color, Field};

pub use palette::{EmptyPalette, Palette};

/// How long a scene takes to return to where it started, at speed 1.
///
/// Ten minutes. Long enough that the repeat is not noticeable -- nobody
/// watches a wallpaper for ten minutes and remembers what it looked like -- and
/// short enough that every scene's slowest component still moves visibly
/// within a minute or two of being looked at.
pub const LOOP_SECONDS: f64 = 600.0;

/// The long edge of the field a scene renders into, unless it asks otherwise.
///
/// 480 pixels. On a 1080p screen that is a fourfold upscale and on a 4K screen
/// an eightfold one, and none of these scenes contains anything sharp enough
/// to notice either. Raising it costs quadratically and buys nothing.
pub const FIELD_EDGE: u32 = 480;

/// Which background to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Kind {
    /// A slowly drifting, slowly rotating gradient. The cheapest, and the
    /// default: it is the one that is unobjectionable behind anything.
    #[default]
    Gradient,
    /// Curtains of light over a dark sky. The most expensive.
    Aurora,
    /// Sums of sines through the palette. The loudest.
    Plasma,
    /// A drifting, twinkling starfield. The only scene with a detail pass.
    Starfield,
}

impl Kind {
    /// Every scene, in the order the documentation lists them.
    pub const ALL: [Self; 4] = [Self::Gradient, Self::Aurora, Self::Plasma, Self::Starfield];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Gradient => "gradient",
            Self::Aurora => "aurora",
            Self::Plasma => "plasma",
            Self::Starfield => "starfield",
        }
    }

    /// One line about what it looks like, for `ravencanvas scenes`.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Self::Gradient => "a slowly drifting, slowly rotating gradient",
            Self::Aurora => "curtains of light over a dark sky",
            Self::Plasma => "interfering waves through the palette",
            Self::Starfield => "a drifting, twinkling starfield",
        }
    }

    /// The long edge of this scene's field.
    #[must_use]
    pub const fn field_edge(self) -> u32 {
        match self {
            Self::Aurora => aurora::FIELD_EDGE,
            _ => FIELD_EDGE,
        }
    }

    /// The palette used when the config names a scene and no colours.
    ///
    /// Every one of these is built from the desktop's own tokens -- huginn's
    /// `#16161F` background and `#7AA2F7` accent, and the darker `#0D0D14` the
    /// login screen falls off to -- so an unconfigured Raven desktop looks like
    /// one thing rather than like four unrelated screensavers.
    #[must_use]
    pub fn default_palette(self) -> Palette {
        let stops = match self {
            Self::Gradient => vec![0xFF0D_0D14, 0xFF16_1622, 0xFF22_243A, 0xFF2E_3355],
            Self::Aurora => vec![0xFF0A_0A12, 0xFF7A_A2F7, 0xFF7A_DEA8, 0xFF9A_7AF7],
            Self::Plasma => vec![0xFF16_161F, 0xFF7A_A2F7, 0xFF2A_2A3A, 0xFF9A_7AF7],
            Self::Starfield => vec![0xFF08_0810, 0xFF12_1424, 0xFF1E_2038, 0xFFD0_D0E0],
        };
        // Cannot fail: every arm above is non-empty.
        Palette::new(stops.into_iter().map(Color::from_argb).collect())
            .unwrap_or_else(|_| Palette::solid(Color::BLACK))
    }
}

impl FromStr for Kind {
    type Err = ParseKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| ParseKindError(s.to_string()))
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A string that is not the name of a scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseKindError(String);

impl fmt::Display for ParseKindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
        write!(
            f,
            "{:?} is not a scene; expected one of {}",
            self.0,
            names.join(", ")
        )
    }
}

impl std::error::Error for ParseKindError {}

/// A scene, its colours and how fast it runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Scene {
    kind: Kind,
    palette: Palette,
    speed: f32,
}

impl Scene {
    /// A scene with an explicit palette and speed.
    ///
    /// `speed` multiplies the rate of everything in the scene. It is not
    /// clamped to positive: a negative speed runs the loop backwards, which is
    /// meaningless for most of these and looks right for a starfield.
    #[must_use]
    pub fn new(kind: Kind, palette: Palette, speed: f32) -> Self {
        Self {
            kind,
            palette,
            // A non-finite speed would make the phase non-finite and every
            // pixel of the scene `NaN`, which reaches the screen as black.
            speed: if speed.is_finite() { speed } else { 1.0 },
        }
    }

    /// A scene with its stock palette, at speed 1.
    #[must_use]
    pub fn with_defaults(kind: Kind) -> Self {
        Self::new(kind, kind.default_palette(), 1.0)
    }

    #[must_use]
    pub const fn kind(&self) -> Kind {
        self.kind
    }

    #[must_use]
    pub const fn palette(&self) -> &Palette {
        &self.palette
    }

    #[must_use]
    pub const fn speed(&self) -> f32 {
        self.speed
    }

    /// Whether this scene changes over time.
    ///
    /// A speed of zero is not a degenerate case to be guarded against; it is a
    /// supported and rather good configuration. It gives a procedural
    /// wallpaper that is drawn once and then costs exactly what a static
    /// image costs, which is nothing -- the engine stops asking for frames.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        self.speed != 0.0
    }

    /// Where in the loop `time` seconds is, in `0.0..1.0`.
    ///
    /// The reduction happens in `f64` and the result is `f32`, which is the
    /// whole point: the precision is spent once here rather than accumulating
    /// in every scene's arithmetic for as long as the session lasts. See the
    /// crate documentation.
    #[must_use]
    pub fn phase(&self, time: f64) -> f32 {
        if !time.is_finite() {
            return 0.0;
        }
        ((time * f64::from(self.speed)) / LOOP_SECONDS).rem_euclid(1.0) as f32
    }

    /// The long edge of the field this scene wants.
    #[must_use]
    pub const fn field_edge(&self) -> u32 {
        self.kind.field_edge()
    }

    /// Draw one frame.
    ///
    /// `field` is the caller's scratch buffer, resized here if the surface
    /// changed. It is passed in rather than allocated because this is called
    /// once per frame per screen, and allocating a scene buffer per frame
    /// would be the only allocation in the render loop.
    pub fn render(&self, canvas: &mut Canvas<'_>, field: &mut Field, time: f64) {
        self.render_at(canvas, field, time, self.field_edge());
    }

    /// Draw one frame into a field of a given size.
    ///
    /// The escape hatch behind `canvas.toml`'s `render.detail`: a machine
    /// where a scene costs too much can be told to draw it smaller, and one
    /// with cycles to spare can be told to draw it larger. The cost is
    /// quadratic in this number, which is what makes it the right dial and the
    /// only one worth exposing.
    pub fn render_at(
        &self,
        canvas: &mut Canvas<'_>,
        field: &mut Field,
        time: f64,
        field_edge: u32,
    ) {
        if canvas.width() <= 0 || canvas.height() <= 0 {
            return;
        }

        let phase = self.phase(time);
        field.resize_for(canvas.width(), canvas.height(), field_edge);

        match self.kind {
            Kind::Gradient => gradient::paint(field, &self.palette, phase),
            Kind::Aurora => aurora::paint(field, &self.palette, phase),
            Kind::Plasma => plasma::paint(field, &self.palette, phase),
            Kind::Starfield => starfield::paint(field, &self.palette, phase),
        }

        field.upscale_into(canvas);

        if self.kind == Kind::Starfield {
            starfield::detail(canvas, &self.palette, phase);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a scene at `time` and return the pixels.
    fn render(scene: &Scene, width: i32, height: i32, time: f64) -> Vec<u8> {
        let mut data = vec![0u8; (width * height * 4) as usize];
        let mut field = Field::for_surface(width, height, scene.field_edge());
        scene.render(&mut Canvas::new(&mut data, width, height), &mut field, time);
        data
    }

    /// The largest per-channel difference between two frames.
    fn difference(a: &[u8], b: &[u8]) -> u8 {
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0)
    }

    // -- naming -------------------------------------------------------------

    #[test]
    fn scene_names_round_trip_and_ignore_case() {
        for kind in Kind::ALL {
            assert_eq!(kind.to_string().parse::<Kind>().unwrap(), kind);
            assert_eq!(kind.name().to_uppercase().parse::<Kind>().unwrap(), kind);
        }
        assert_eq!(Kind::default(), Kind::Gradient);
    }

    #[test]
    fn an_unknown_scene_names_the_ones_that_exist() {
        let error = "fireplace".parse::<Kind>().unwrap_err().to_string();
        assert!(
            error.contains("gradient") && error.contains("starfield"),
            "{error}"
        );
        assert!(error.contains("fireplace"), "{error}");
    }

    #[test]
    fn every_scene_has_a_usable_default_palette() {
        for kind in Kind::ALL {
            assert!(kind.default_palette().len() >= 2, "{kind}");
            assert!(!kind.summary().is_empty(), "{kind}");
        }
    }

    // -- the phase ----------------------------------------------------------

    #[test]
    fn the_phase_wraps_once_a_loop() {
        let scene = Scene::with_defaults(Kind::Plasma);
        assert_eq!(scene.phase(0.0), 0.0);
        assert_eq!(scene.phase(LOOP_SECONDS), 0.0);
        assert_eq!(scene.phase(LOOP_SECONDS / 4.0), 0.25);
        assert_eq!(scene.phase(LOOP_SECONDS * 9.5), 0.5);
    }

    #[test]
    fn speed_scales_the_phase_and_may_be_negative() {
        let fast = Scene::new(Kind::Plasma, Kind::Plasma.default_palette(), 2.0);
        assert_eq!(fast.phase(LOOP_SECONDS / 4.0), 0.5);

        let backwards = Scene::new(Kind::Plasma, Kind::Plasma.default_palette(), -1.0);
        assert_eq!(backwards.phase(LOOP_SECONDS / 4.0), 0.75);
    }

    #[test]
    fn a_speed_of_zero_freezes_the_phase() {
        let still = Scene::new(Kind::Aurora, Kind::Aurora.default_palette(), 0.0);
        assert!(!still.is_animated());
        for time in [0.0, 1.0, 5_000.0, 8.6e7] {
            assert_eq!(still.phase(time), 0.0, "at {time}s");
        }
    }

    /// The reason the phase exists. After a fortnight, seconds-as-`f32` cannot
    /// resolve a frame; a phase can, because it never leaves `0.0..1.0`.
    #[test]
    fn the_phase_still_advances_after_a_month_of_uptime() {
        let scene = Scene::with_defaults(Kind::Plasma);
        let month = 30.0 * 24.0 * 3600.0;
        let one_frame = 1.0 / 60.0;
        assert_ne!(
            scene.phase(month),
            scene.phase(month + one_frame),
            "a month in, one frame of time made no difference to the phase"
        );
    }

    #[test]
    fn a_nonsense_time_or_speed_does_not_produce_a_nan_phase() {
        let scene = Scene::with_defaults(Kind::Plasma);
        assert_eq!(scene.phase(f64::NAN), 0.0);
        assert_eq!(scene.phase(f64::INFINITY), 0.0);
        // A non-finite speed is refused at construction rather than carried.
        assert_eq!(
            Scene::new(Kind::Plasma, Palette::solid(Color::BLACK), f32::NAN).speed(),
            1.0
        );
    }

    // -- rendering ----------------------------------------------------------

    #[test]
    fn every_scene_fills_the_whole_surface_opaquely() {
        for kind in Kind::ALL {
            let frame = render(&Scene::with_defaults(kind), 96, 54, 12.0);
            assert!(
                frame.chunks_exact(4).all(|p| p[3] == 0xFF),
                "{kind} left a hole"
            );
        }
    }

    #[test]
    fn every_scene_is_deterministic() {
        for kind in Kind::ALL {
            let scene = Scene::with_defaults(kind);
            assert_eq!(
                render(&scene, 64, 40, 33.0),
                render(&scene, 64, 40, 33.0),
                "{kind}"
            );
        }
    }

    /// The property the whole `phase` design exists to provide: the last frame
    /// of a loop and the first frame of the next are the same picture, so the
    /// wallpaper never jumps.
    #[test]
    fn every_scene_returns_to_its_starting_frame_after_one_loop() {
        for kind in Kind::ALL {
            let scene = Scene::with_defaults(kind);
            let start = render(&scene, 80, 48, 0.0);
            let looped = render(&scene, 80, 48, LOOP_SECONDS);
            // Within one level rather than byte-identical: `sin` of `TAU` is
            // not exactly `sin` of zero in binary floating point, and the
            // difference is far below what quantizing to eight bits can show.
            let seam = difference(&start, &looped);
            assert!(seam <= 1, "{kind} jumps by {seam} levels at the loop point");
        }
    }

    #[test]
    fn every_scene_actually_animates() {
        for kind in Kind::ALL {
            let scene = Scene::with_defaults(kind);
            let early = render(&scene, 80, 48, 0.0);
            let later = render(&scene, 80, 48, LOOP_SECONDS / 8.0);
            assert!(
                difference(&early, &later) > 8,
                "{kind} barely moved over an eighth of its loop"
            );
        }
    }

    #[test]
    fn a_scene_at_zero_speed_draws_the_same_frame_forever() {
        for kind in Kind::ALL {
            let still = Scene::new(kind, kind.default_palette(), 0.0);
            assert_eq!(
                render(&still, 64, 40, 0.0),
                render(&still, 64, 40, 9_999.0),
                "{kind}"
            );
        }
    }

    /// A scene has to look like its palette. This is the check that a
    /// configured colour actually reaches the screen rather than being
    /// overwhelmed by something compiled in.
    ///
    /// [`Kind::Aurora`] is not in the list because it is additive by
    /// construction; it gets its own test below.
    #[test]
    fn a_single_colour_palette_paints_that_colour() {
        for kind in [Kind::Gradient, Kind::Plasma, Kind::Starfield] {
            let flat = Scene::new(kind, Palette::solid(Color::rgb(0x40, 0x80, 0xC0)), 1.0);
            // A surface large enough that the starfield's minimum star count
            // is sparse on it. On a 64x40 test surface those 120 stars cover a
            // fifth of the screen, which says nothing about the scene.
            let frame = render(&flat, 256, 160, 5.0);
            // The starfield draws white stars over its sky, so a handful of
            // pixels are allowed to be something else; the sky must not be.
            let matching = frame
                .chunks_exact(4)
                .filter(|p| p[2].abs_diff(0x40) <= 2 && p[0].abs_diff(0xC0) <= 2)
                .count();
            let total = frame.len() / 4;
            assert!(
                matching * 100 / total >= 90,
                "{kind}: only {matching} of {total} pixels were the palette colour"
            );
        }
    }

    /// The aurora's first stop is the *sky*, and its curtains add light on top
    /// of it rather than replacing it. So the property to check is not that
    /// the screen is the palette colour but that it is never darker than it,
    /// and that the sky is still visible where no curtain reaches.
    #[test]
    fn the_aurora_adds_light_to_its_sky_rather_than_replacing_it() {
        let sky = Color::rgb(0x20, 0x30, 0x40);
        let frame = render(
            &Scene::new(Kind::Aurora, Palette::solid(sky), 1.0),
            128,
            80,
            5.0,
        );
        let reds: Vec<u8> = frame.chunks_exact(4).map(|p| p[2]).collect();

        let darkest = *reds.iter().min().expect("a screen has pixels");
        assert!(
            darkest >= 0x1E,
            "something was darker than the sky: {darkest:#04x}"
        );
        assert!(
            reds.iter().any(|&r| r <= 0x22),
            "the sky is never visible; the curtains have swallowed the whole screen"
        );
        assert!(
            reds.iter().any(|&r| r > 0x2A),
            "no curtain is brighter than the sky; the aurora drew nothing"
        );
    }

    #[test]
    fn the_starfield_draws_stars_over_its_sky() {
        // A dark sky and white stars: the detail pass must produce pixels
        // brighter than anything the field could have.
        let sky = Palette::new(vec![Color::BLACK, Color::rgb(0xFF, 0xFF, 0xFF)]).unwrap();
        let frame = render(&Scene::new(Kind::Starfield, sky, 1.0), 320, 200, 3.0);
        let bright = frame.chunks_exact(4).filter(|p| p[2] > 0x60).count();
        assert!(
            bright > 20,
            "only {bright} lit pixels; the detail pass drew nothing"
        );
        assert!(
            bright < frame.len() / 4 / 4,
            "stars covered a quarter of the screen; that is not a starfield"
        );
    }

    #[test]
    fn only_the_starfield_asks_for_a_smaller_field_than_the_default() {
        assert_eq!(Kind::Gradient.field_edge(), FIELD_EDGE);
        assert!(
            Kind::Aurora.field_edge() < FIELD_EDGE,
            "aurora is the expensive one"
        );
    }

    #[test]
    fn the_detail_override_changes_the_field_it_draws_into() {
        let scene = Scene::with_defaults(Kind::Gradient);
        let mut coarse = Field::new(1, 1);
        let mut fine = Field::new(1, 1);
        let mut data = vec![0u8; 64 * 64 * 4];

        scene.render_at(&mut Canvas::new(&mut data, 64, 64), &mut coarse, 0.0, 8);
        scene.render_at(&mut Canvas::new(&mut data, 64, 64), &mut fine, 0.0, 480);

        assert_eq!((coarse.width(), coarse.height()), (8, 8));
        assert_eq!(
            (fine.width(), fine.height()),
            (64, 64),
            "capped by the surface"
        );
    }

    #[test]
    fn rendering_an_odd_or_empty_surface_does_not_panic() {
        for kind in Kind::ALL {
            let scene = Scene::with_defaults(kind);
            for (w, h) in [(1, 1), (1, 400), (400, 1), (7, 3)] {
                let frame = render(&scene, w, h, 1.0);
                assert_eq!(frame.len(), (w * h * 4) as usize, "{kind} at {w}x{h}");
            }
            // And a zero-sized surface leaves the buffer alone rather than
            // reaching into it.
            let mut data = vec![0u8; 4];
            let mut field = Field::new(4, 4);
            scene.render(&mut Canvas::new(&mut data, 0, 0), &mut field, 1.0);
        }
    }
}
