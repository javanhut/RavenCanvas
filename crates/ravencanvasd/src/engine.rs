//! What should be on the screen, and when it should change.
//!
//! Everything about *time* in this daemon is here, and nothing about Wayland
//! is. That split is what makes the interesting behaviour testable: whether a
//! slideshow advances when it should, whether a crossfade ends exactly at its
//! destination, whether pausing stops the clock rather than merely stopping
//! the drawing -- all of it is answerable with a fabricated [`Instant`] and no
//! compositor.
//!
//! # The virtual clock
//!
//! No part of this measures elapsed time directly. Everything is measured
//! against a clock that stops when the daemon is paused, so a scene resumes
//! from where it froze rather than jumping forward by however long it was
//! paused, and a slideshow that was paused for an hour does not immediately
//! advance three times when it comes back. One clock, one behaviour, and no
//! per-feature pause handling to keep in step.
//!
//! # Failures do not clear the screen
//!
//! A wallpaper that cannot be loaded leaves the last one up. That rule runs
//! through the whole module: a slideshow entry that will not decode is skipped
//! and the next tried; a directory that has become unreadable keeps the image
//! already showing; a config that names a missing file changes nothing but a
//! log line. The one outcome to avoid is a desktop that goes blank because
//! somebody deleted a JPEG.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use raven_canvas_proto::Background;
use raven_paint::{Color, Fit, Image};
use raven_scene::Scene;

use crate::config::Render;
use crate::playlist::Playlist;
use crate::resolve::{self, Plan};

/// How many entries a slideshow will try before giving up on an advance.
///
/// A directory with one corrupt file in it should skip past it silently. A
/// directory with a thousand should not spend a minute of CPU discovering
/// that at every tick.
const MAX_LOAD_ATTEMPTS: usize = 8;

/// A clock that stops when the daemon is paused.
#[derive(Debug, Clone, Copy)]
struct Clock {
    /// Virtual time already banked, from before the current run.
    banked: Duration,
    /// When the current run started, in real time. `None` while paused.
    running_since: Option<Instant>,
}

impl Clock {
    fn started(now: Instant) -> Self {
        Self {
            banked: Duration::ZERO,
            running_since: Some(now),
        }
    }

    fn now(&self, real: Instant) -> Duration {
        match self.running_since {
            // `saturating_duration_since`, because a caller may hand us an
            // `Instant` from before the clock started -- the control socket
            // and the event loop both take "now" independently.
            Some(since) => self.banked + real.saturating_duration_since(since),
            None => self.banked,
        }
    }

    fn set_paused(&mut self, paused: bool, real: Instant) {
        match (paused, self.running_since) {
            (true, Some(since)) => {
                self.banked += real.saturating_duration_since(since);
                self.running_since = None;
            }
            (false, None) => self.running_since = Some(real),
            _ => {}
        }
    }

    fn is_paused(&self) -> bool {
        self.running_since.is_none()
    }
}

/// A decoded image, and the number that identifies it to a cache.
#[derive(Debug)]
struct Slide {
    /// Unique for the lifetime of the process. Screens key their scaled copies
    /// on this, so a slideshow returning to an image it showed before still
    /// counts as a different image and is rescaled -- which is correct, and
    /// cheaper to reason about than proving it is not.
    id: u64,
    path: PathBuf,
    image: Image,
}

/// A crossfade in progress.
#[derive(Debug, Clone, Copy)]
struct Transition {
    started: Duration,
    duration: Duration,
}

impl Transition {
    /// How far through, `0.0..=1.0`.
    fn progress(&self, now: Duration) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_sub(self.started);
        (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }
}

/// What a screen should draw this frame.
///
/// Borrowed from the engine rather than cloned: the image variants carry
/// megabytes, and this is produced once per screen per frame.
#[derive(Debug)]
pub(crate) enum Frame<'a> {
    /// Nothing has been loaded yet, or everything failed. The flat colour is
    /// the floor this daemon never falls through.
    Color(Color),
    Image {
        id: u64,
        image: &'a Image,
        fit: Fit,
        background: Color,
    },
    Crossfade {
        from: (u64, &'a Image),
        to: (u64, &'a Image),
        fit: Fit,
        background: Color,
        /// `0.0` is entirely `from`, `1.0` entirely `to`.
        progress: f32,
    },
    Scene {
        scene: &'a Scene,
        /// Seconds on the virtual clock.
        time: f64,
    },
}

/// The daemon's idea of what is on screen.
#[derive(Debug)]
pub(crate) struct Engine {
    background: Background,
    render: Render,
    plan: Plan,
    clock: Clock,

    playlist: Playlist,
    /// When the image on screen started being shown.
    slide_since: Duration,
    transition: Option<Transition>,

    current: Option<Slide>,
    previous: Option<Slide>,
    next_id: u64,

    /// When the last frame was drawn, for the frame-rate cap.
    last_frame: Duration,
    frames: u64,
}

impl Engine {
    /// Build an engine for `background`.
    ///
    /// A background that does not resolve is reported and replaced by the
    /// fallback colour, so construction cannot fail. That is the module's rule
    /// applied to startup: a broken config at boot should give a plain desktop
    /// and a log line, not a daemon that exits and leaves nothing at all.
    pub(crate) fn new(background: Background, render: Render, now: Instant) -> Self {
        let mut engine = Self {
            background: Background::Color {
                color: FALLBACK.to_string(),
            },
            render,
            plan: Plan::Color(fallback_colour()),
            clock: Clock::started(now),
            playlist: Playlist::new(Vec::new(), false, 0),
            slide_since: Duration::ZERO,
            transition: None,
            current: None,
            previous: None,
            next_id: 1,
            last_frame: Duration::ZERO,
            frames: 0,
        };
        if let Err(e) = engine.apply(background, render, now) {
            // The fallback colour set above stays. This is the module's rule
            // applied to startup: a background that will not resolve gives a
            // plain desktop and a log line, not a daemon that exits and leaves
            // nothing at all.
            tracing::warn!("falling back to {FALLBACK}: {e:#}");
        }
        engine
    }

    /// Show something else.
    ///
    /// Returns an error, and changes nothing, if the background does not
    /// resolve -- that is what lets the control socket refuse a bad request
    /// without having already half-applied it.
    pub(crate) fn apply(
        &mut self,
        background: Background,
        render: Render,
        now: Instant,
    ) -> anyhow::Result<()> {
        let plan = resolve::plan(&background)?;

        // An identical background is a no-op rather than a restart. This is
        // not an optimization: the config watcher fires on every write to the
        // file, including writes that changed a comment, and restarting a
        // slideshow's timer or a scene's phase each time would be visible.
        if self.background == background {
            self.render = render;
            return Ok(());
        }

        self.background = background;
        self.render = render;
        self.plan = plan;
        self.transition = None;
        self.previous = None;

        let virtual_now = self.clock.now(now);
        self.slide_since = virtual_now;
        self.last_frame = Duration::ZERO;

        match (&self.plan, &self.background) {
            (Plan::Image { .. }, Background::Image { path, .. }) => {
                let path = path.clone();
                self.load(&path);
            }
            (Plan::Slideshow { shuffle, .. }, Background::Slideshow { directory, .. }) => {
                let (shuffle, directory) = (*shuffle, directory.clone());
                self.rescan(&directory, shuffle);
                self.load_from_playlist();
            }
            _ => {
                self.playlist = Playlist::new(Vec::new(), false, 0);
                self.current = None;
            }
        }
        Ok(())
    }

    pub(crate) fn background(&self) -> &Background {
        &self.background
    }

    pub(crate) fn render(&self) -> Render {
        self.render
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.clock.is_paused()
    }

    pub(crate) fn is_animated(&self) -> bool {
        self.plan.is_animated()
    }

    pub(crate) fn frames(&self) -> u64 {
        self.frames
    }

    pub(crate) fn playlist_len(&self) -> usize {
        self.playlist.len()
    }

    pub(crate) fn current_image(&self) -> Option<&Path> {
        self.current.as_ref().map(|slide| slide.path.as_path())
    }

    /// Stop or restart the clock. Returns whether anything changed.
    pub(crate) fn set_paused(&mut self, paused: bool, now: Instant) -> bool {
        if paused == self.clock.is_paused() {
            return false;
        }
        self.clock.set_paused(paused, now);
        true
    }

    /// Move a slideshow along by hand. Returns whether the screen changed.
    pub(crate) fn advance(&mut self, by: i32, now: Instant) -> bool {
        if !matches!(self.plan, Plan::Slideshow { .. }) || self.playlist.is_empty() {
            return false;
        }
        self.playlist.advance(by);
        self.begin_slide(now)
    }

    /// Re-read the slideshow directory, if that is what is being shown.
    ///
    /// Called when the directory changes underneath us. Keeps the image that
    /// is on screen if it is still there; see [`Playlist::replace`].
    pub(crate) fn rescan_directory(&mut self) -> bool {
        let Plan::Slideshow { .. } = self.plan else {
            return false;
        };
        let Background::Slideshow { directory, .. } = &self.background else {
            return false;
        };

        let directory = directory.clone();
        let before = self.playlist.len();
        match Playlist::scan(&directory) {
            Ok(entries) => {
                self.playlist.replace(entries);
                let changed = self.playlist.len() != before;
                if changed {
                    tracing::info!(
                        directory = %directory.display(),
                        was = before,
                        now = self.playlist.len(),
                        "the slideshow directory changed"
                    );
                }
                // If nothing was loadable before and there is something now --
                // the directory was empty at startup and has been filled --
                // take it immediately rather than waiting for the interval.
                if self.current.is_none() && !self.playlist.is_empty() {
                    return self.load_from_playlist();
                }
                changed
            }
            Err(e) => {
                tracing::warn!(
                    directory = %directory.display(),
                    "cannot rescan the slideshow directory: {e}"
                );
                false
            }
        }
    }

    /// Let time pass. Returns whether the screens should redraw.
    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        if self.clock.is_paused() {
            return false;
        }
        let virtual_now = self.clock.now(now);

        // A slide that is due. Checked before the frame cap, because this is a
        // discrete event and must not be dropped by it.
        let mut redraw = false;
        if let Plan::Slideshow { interval, .. } = self.plan
            && !self.playlist.is_empty()
            && virtual_now.saturating_sub(self.slide_since) >= interval
        {
            self.playlist.advance(1);
            redraw |= self.begin_slide(now);
        }

        // A transition that has finished. Also discrete: the final frame has
        // to be drawn at exactly `1.0`, or the cut back to a plain blit shows.
        if let Some(transition) = self.transition
            && transition.progress(virtual_now) >= 1.0
        {
            self.transition = None;
            self.previous = None;
            redraw = true;
        }

        // Continuous animation, which is what the frame cap is for.
        let animating = self.transition.is_some()
            || matches!(&self.plan, Plan::Scene(scene) if scene.is_animated());
        if animating && virtual_now.saturating_sub(self.last_frame) >= self.render.frame_interval()
        {
            redraw = true;
        }

        redraw
    }

    /// How long the event loop may sleep, or `None` to sleep until something
    /// happens.
    ///
    /// This is the whole of the daemon's idle behaviour. A static wallpaper
    /// returns `None` and the process blocks on its descriptors indefinitely,
    /// costing nothing at all -- not a timer, not a wakeup, not a scheduler
    /// entry. That is the number that matters for a program which is running
    /// on every desktop all the time.
    pub(crate) fn next_wake(&self, now: Instant) -> Option<Duration> {
        if self.clock.is_paused() {
            return None;
        }
        let virtual_now = self.clock.now(now);
        let interval = self.render.frame_interval();

        let animating = self.transition.is_some()
            || matches!(&self.plan, Plan::Scene(scene) if scene.is_animated());
        let frame =
            animating.then(|| interval.saturating_sub(virtual_now.saturating_sub(self.last_frame)));

        let slide = match self.plan {
            Plan::Slideshow { interval, .. } if !self.playlist.is_empty() => {
                Some(interval.saturating_sub(virtual_now.saturating_sub(self.slide_since)))
            }
            // A slideshow with nothing in it still wakes, slowly, to look
            // again: the directory watcher covers a directory that exists, and
            // this covers one that does not yet.
            Plan::Slideshow { .. } => Some(Duration::from_secs(30)),
            _ => None,
        };

        match (frame, slide) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (some, None) | (None, some) => some,
        }
    }

    /// Record that a frame was drawn.
    pub(crate) fn note_frame(&mut self, now: Instant, screens: u64) {
        self.last_frame = self.clock.now(now);
        self.frames += screens;
    }

    /// What to draw.
    pub(crate) fn frame(&self, now: Instant) -> Frame<'_> {
        let virtual_now = self.clock.now(now);

        match &self.plan {
            Plan::Color(colour) => Frame::Color(*colour),
            Plan::Scene(scene) => Frame::Scene {
                scene,
                time: virtual_now.as_secs_f64(),
            },
            Plan::Image { fit, background }
            | Plan::Slideshow {
                fit, background, ..
            } => {
                let Some(current) = &self.current else {
                    // Nothing has loaded. The letterbox colour is the right
                    // fallback rather than black: it is what the edges of this
                    // wallpaper were going to be anyway.
                    return Frame::Color(*background);
                };

                match (&self.transition, &self.previous) {
                    (Some(transition), Some(previous)) => Frame::Crossfade {
                        from: (previous.id, &previous.image),
                        to: (current.id, &current.image),
                        fit: *fit,
                        background: *background,
                        progress: transition.progress(virtual_now),
                    },
                    _ => Frame::Image {
                        id: current.id,
                        image: &current.image,
                        fit: *fit,
                        background: *background,
                    },
                }
            }
        }
    }

    // -- loading ------------------------------------------------------------

    /// Start showing whatever the playlist is pointing at, with a crossfade if
    /// one is configured.
    fn begin_slide(&mut self, now: Instant) -> bool {
        let previous = self.current.take();
        let loaded = self.load_from_playlist();

        self.slide_since = self.clock.now(now);
        if !loaded {
            // Nothing loadable. Put back what was on screen rather than
            // leaving the desktop empty.
            self.current = previous;
            return false;
        }

        let crossfade = match self.plan {
            Plan::Slideshow { crossfade, .. } => crossfade,
            _ => Duration::ZERO,
        };
        if crossfade.is_zero() || previous.is_none() {
            self.previous = None;
            self.transition = None;
        } else {
            self.previous = previous;
            self.transition = Some(Transition {
                started: self.slide_since,
                duration: crossfade,
            });
        }
        true
    }

    /// Load the playlist's current entry, skipping ones that will not decode.
    fn load_from_playlist(&mut self) -> bool {
        for attempt in 0..MAX_LOAD_ATTEMPTS.min(self.playlist.len().max(1)) {
            let Some(path) = self.playlist.current().map(Path::to_path_buf) else {
                return false;
            };
            if self.load(&path) {
                return true;
            }
            // That one is broken; try the next. `advance` here rather than a
            // separate index because it keeps the playlist's own idea of where
            // it is correct -- the broken file really has been passed over.
            if attempt + 1 < MAX_LOAD_ATTEMPTS {
                self.playlist.advance(1);
            }
        }
        tracing::warn!("nothing in the slideshow directory would decode");
        false
    }

    /// Decode one file into [`Engine::current`].
    fn load(&mut self, path: &Path) -> bool {
        match Image::load(path) {
            Ok(image) => {
                let id = self.next_id;
                self.next_id += 1;
                tracing::info!(
                    path = %path.display(),
                    width = image.width(),
                    height = image.height(),
                    "showing"
                );
                self.current = Some(Slide {
                    id,
                    path: path.to_path_buf(),
                    image,
                });
                true
            }
            Err(e) => {
                tracing::warn!("{e:#}");
                false
            }
        }
    }

    fn rescan(&mut self, directory: &Path, shuffle: bool) {
        let entries = match Playlist::scan(directory) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    directory = %directory.display(),
                    "cannot read the slideshow directory: {e}"
                );
                Vec::new()
            }
        };
        tracing::info!(
            directory = %directory.display(),
            images = entries.len(),
            "slideshow"
        );
        self.playlist = Playlist::new(entries, shuffle, shuffle_seed());
    }
}

/// huginn's `BACKGROUND`, as the last thing this daemon will fall back to.
const FALLBACK: &str = "#16161F";

fn fallback_colour() -> Color {
    FALLBACK.parse().unwrap_or(Color::BLACK)
}

/// A seed for the first shuffle of a session.
///
/// The wall clock, because the point is only that two boots do not open on the
/// same picture. Nothing depends on this being unpredictable, and everything
/// downstream of it is deterministic given the seed -- which is what
/// `playlist`'s tests rely on.
fn shuffle_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use raven_canvas_proto::Background;

    fn render() -> Render {
        Render { fps: 30, detail: 0 }
    }

    fn scene(name: &str, speed: f32) -> Background {
        Background::Scene {
            name: name.into(),
            speed,
            palette: Vec::new(),
        }
    }

    fn colour(hex: &str) -> Background {
        Background::Color { color: hex.into() }
    }

    // -- the clock ----------------------------------------------------------

    #[test]
    fn the_clock_runs_forward() {
        let start = Instant::now();
        let clock = Clock::started(start);
        assert_eq!(clock.now(start), Duration::ZERO);
        assert_eq!(
            clock.now(start + Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    /// The property that makes pausing work everywhere at once: a paused clock
    /// does not advance, and resuming continues from where it stopped rather
    /// than jumping to real time.
    #[test]
    fn a_paused_clock_stops_and_resumes_where_it_left_off() {
        let start = Instant::now();
        let mut clock = Clock::started(start);

        clock.set_paused(true, start + Duration::from_secs(10));
        assert_eq!(
            clock.now(start + Duration::from_secs(10)),
            Duration::from_secs(10)
        );
        assert_eq!(
            clock.now(start + Duration::from_secs(600)),
            Duration::from_secs(10),
            "time passed while paused"
        );

        clock.set_paused(false, start + Duration::from_secs(600));
        assert_eq!(
            clock.now(start + Duration::from_secs(601)),
            Duration::from_secs(11),
            "resuming jumped forward by the paused interval"
        );
    }

    #[test]
    fn pausing_twice_is_not_two_pauses() {
        let start = Instant::now();
        let mut clock = Clock::started(start);
        clock.set_paused(true, start + Duration::from_secs(1));
        clock.set_paused(true, start + Duration::from_secs(9));
        clock.set_paused(false, start + Duration::from_secs(9));
        assert_eq!(
            clock.now(start + Duration::from_secs(10)),
            Duration::from_secs(2)
        );
    }

    // -- transitions --------------------------------------------------------

    #[test]
    fn a_transition_runs_from_zero_to_one_and_stops() {
        let transition = Transition {
            started: Duration::from_secs(10),
            duration: Duration::from_millis(800),
        };
        assert_eq!(transition.progress(Duration::from_secs(10)), 0.0);
        assert_eq!(transition.progress(Duration::from_millis(10_400)), 0.5);
        assert_eq!(transition.progress(Duration::from_millis(10_800)), 1.0);
        assert_eq!(transition.progress(Duration::from_secs(30)), 1.0);
    }

    #[test]
    fn a_zero_length_transition_is_already_finished() {
        let cut = Transition {
            started: Duration::from_secs(1),
            duration: Duration::ZERO,
        };
        assert_eq!(cut.progress(Duration::from_secs(1)), 1.0);
    }

    // -- the engine ---------------------------------------------------------

    #[test]
    fn a_colour_background_needs_no_wakeups_at_all() {
        let now = Instant::now();
        let engine = Engine::new(colour("#7AA2F7"), render(), now);
        assert!(!engine.is_animated());
        assert_eq!(
            engine.next_wake(now),
            None,
            "a flat colour must let the process sleep"
        );
        assert!(matches!(engine.frame(now), Frame::Color(_)));
    }

    #[test]
    fn a_frozen_scene_needs_no_wakeups_either() {
        let now = Instant::now();
        let engine = Engine::new(scene("plasma", 0.0), render(), now);
        assert_eq!(engine.next_wake(now), None);
        assert!(matches!(engine.frame(now), Frame::Scene { .. }));
    }

    #[test]
    fn an_animated_scene_asks_to_be_woken_at_the_frame_rate() {
        let now = Instant::now();
        let engine = Engine::new(scene("aurora", 1.0), render(), now);
        assert!(engine.is_animated());
        assert_eq!(engine.next_wake(now), Some(render().frame_interval()));
    }

    #[test]
    fn an_animated_scene_asks_for_a_redraw_once_a_frame_and_not_more() {
        let start = Instant::now();
        let mut engine = Engine::new(scene("plasma", 1.0), render(), start);

        // The very first frame is drawn when the surface is configured, not by
        // polling, so nothing is due at the instant the clock starts.
        assert!(!engine.poll(start));
        assert!(!engine.poll(start + Duration::from_millis(5)), "too soon");

        assert!(
            engine.poll(start + Duration::from_millis(40)),
            "a frame is due"
        );
        engine.note_frame(start + Duration::from_millis(40), 1);

        assert!(
            !engine.poll(start + Duration::from_millis(50)),
            "just drawn"
        );
        assert!(
            engine.poll(start + Duration::from_millis(80)),
            "the next frame is due"
        );
    }

    #[test]
    fn a_paused_engine_stops_asking_for_anything() {
        let start = Instant::now();
        let mut engine = Engine::new(scene("plasma", 1.0), render(), start);
        engine.note_frame(start, 1);

        assert!(engine.set_paused(true, start));
        assert!(
            !engine.set_paused(true, start),
            "pausing twice changes nothing"
        );
        assert!(!engine.poll(start + Duration::from_secs(60)));
        assert_eq!(engine.next_wake(start + Duration::from_secs(60)), None);
        assert!(engine.is_paused());
    }

    /// A paused scene must resume from the frame it froze on, not from
    /// wherever real time got to. This is the visible consequence of the
    /// virtual clock, and the reason it exists.
    #[test]
    fn a_paused_scene_resumes_from_where_it_froze() {
        let start = Instant::now();
        let mut engine = Engine::new(scene("plasma", 1.0), render(), start);

        let Frame::Scene { time: before, .. } = engine.frame(start + Duration::from_secs(5)) else {
            panic!("not a scene");
        };
        engine.set_paused(true, start + Duration::from_secs(5));

        let Frame::Scene { time: during, .. } = engine.frame(start + Duration::from_secs(500))
        else {
            panic!("not a scene");
        };
        assert_eq!(before, during, "the scene moved while paused");

        engine.set_paused(false, start + Duration::from_secs(500));
        let Frame::Scene { time: after, .. } = engine.frame(start + Duration::from_secs(501))
        else {
            panic!("not a scene");
        };
        assert!((after - before - 1.0).abs() < 0.01, "{before} then {after}");
    }

    #[test]
    fn applying_the_same_background_does_not_restart_anything() {
        let start = Instant::now();
        let mut engine = Engine::new(scene("plasma", 1.0), render(), start);
        engine.note_frame(start + Duration::from_secs(5), 1);

        let Frame::Scene { time: before, .. } = engine.frame(start + Duration::from_secs(5)) else {
            panic!()
        };
        engine
            .apply(
                scene("plasma", 1.0),
                render(),
                start + Duration::from_secs(5),
            )
            .expect("apply");
        let Frame::Scene { time: after, .. } = engine.frame(start + Duration::from_secs(5)) else {
            panic!()
        };
        assert_eq!(before, after, "an identical config restarted the scene");
    }

    #[test]
    fn applying_a_bad_background_changes_nothing_and_says_why() {
        let now = Instant::now();
        let mut engine = Engine::new(scene("plasma", 1.0), render(), now);
        let error = engine
            .apply(scene("fireplace", 1.0), render(), now)
            .unwrap_err();

        assert!(format!("{error:#}").contains("fireplace"), "{error:#}");
        assert_eq!(
            engine.background(),
            &scene("plasma", 1.0),
            "it was half applied"
        );
    }

    /// The rule the module header states: a background naming a file that is
    /// not there must not blank the desktop.
    #[test]
    fn an_image_that_cannot_be_loaded_falls_back_to_a_colour_rather_than_nothing() {
        let now = Instant::now();
        let engine = Engine::new(
            Background::Image {
                path: "/nonexistent/wallpaper.png".into(),
                fit: "cover".into(),
                background: "#123456".into(),
            },
            render(),
            now,
        );
        match engine.frame(now) {
            Frame::Color(colour) => assert_eq!(colour, "#123456".parse().unwrap()),
            other => panic!("expected the letterbox colour, got {other:?}"),
        }
        assert_eq!(engine.current_image(), None);
    }

    #[test]
    fn advancing_a_slideshow_that_is_not_running_does_nothing() {
        let now = Instant::now();
        let mut engine = Engine::new(scene("plasma", 1.0), render(), now);
        assert!(!engine.advance(1, now));
        assert!(!engine.rescan_directory());
    }

    #[test]
    fn a_slideshow_with_no_images_still_wakes_up_to_look_again() {
        let now = Instant::now();
        let engine = Engine::new(
            Background::Slideshow {
                directory: "/nonexistent/wallpapers".into(),
                interval: 60,
                shuffle: false,
                crossfade: 0,
                fit: "cover".into(),
                background: "#16161F".into(),
            },
            render(),
            now,
        );
        assert_eq!(engine.playlist_len(), 0);
        assert_eq!(engine.next_wake(now), Some(Duration::from_secs(30)));
    }

    #[test]
    fn frames_are_counted_per_screen() {
        let now = Instant::now();
        let mut engine = Engine::new(colour("#000"), render(), now);
        assert_eq!(engine.frames(), 0);
        engine.note_frame(now, 2);
        engine.note_frame(now, 2);
        assert_eq!(engine.frames(), 4);
    }

    // -- slideshows, against real files -------------------------------------

    /// A directory of real PNGs, removed when the test ends.
    struct Pictures(PathBuf);

    impl Pictures {
        fn new(name: &str, count: usize) -> Self {
            let path = std::env::temp_dir().join(format!("ravencanvas-slides-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch");
            for index in 0..count {
                std::fs::write(path.join(format!("{index:02}.png")), png(index as u8))
                    .expect("write");
            }
            Self(path)
        }

        fn background(&self, interval: u64, crossfade: u64) -> Background {
            Background::Slideshow {
                directory: self.0.clone(),
                interval,
                shuffle: false,
                crossfade,
                fit: "cover".into(),
                background: "#16161F".into(),
            }
        }
    }

    impl Drop for Pictures {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A 1x1 PNG of one grey level, so each file is distinguishable.
    fn png(level: u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .expect("header")
            .write_image_data(&[level, level, level])
            .expect("data");
        out
    }

    #[test]
    fn a_slideshow_starts_on_its_first_image() {
        let pictures = Pictures::new("start", 3);
        let now = Instant::now();
        let engine = Engine::new(pictures.background(60, 0), render(), now);

        assert_eq!(engine.playlist_len(), 3);
        assert_eq!(
            engine.current_image().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("00.png"))
        );
        assert!(matches!(engine.frame(now), Frame::Image { .. }));
    }

    #[test]
    fn a_slideshow_advances_when_its_interval_elapses_and_not_before() {
        let pictures = Pictures::new("advance", 3);
        let start = Instant::now();
        let mut engine = Engine::new(pictures.background(60, 0), render(), start);
        engine.note_frame(start, 1);

        assert!(!engine.poll(start + Duration::from_secs(59)));
        assert_eq!(
            engine.current_image().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("00.png"))
        );

        assert!(engine.poll(start + Duration::from_secs(60)));
        assert_eq!(
            engine.current_image().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("01.png"))
        );
    }

    #[test]
    fn a_slideshow_asks_to_be_woken_when_the_next_slide_is_due() {
        let pictures = Pictures::new("wake", 2);
        let start = Instant::now();
        let engine = Engine::new(pictures.background(60, 0), render(), start);
        assert_eq!(
            engine.next_wake(start + Duration::from_secs(20)),
            Some(Duration::from_secs(40))
        );
    }

    #[test]
    fn a_crossfade_runs_and_then_gets_out_of_the_way() {
        let pictures = Pictures::new("fade", 2);
        let start = Instant::now();
        let mut engine = Engine::new(pictures.background(10, 1_000), render(), start);
        engine.note_frame(start, 1);

        // The slide changes at ten seconds and the fade starts.
        assert!(engine.poll(start + Duration::from_secs(10)));
        match engine.frame(start + Duration::from_millis(10_500)) {
            Frame::Crossfade { progress, .. } => assert!((progress - 0.5).abs() < 0.01),
            other => panic!("expected a crossfade, got {other:?}"),
        }

        // And is torn down at the end, leaving a plain image behind.
        assert!(engine.poll(start + Duration::from_millis(11_000)));
        assert!(matches!(
            engine.frame(start + Duration::from_millis(11_000)),
            Frame::Image { .. }
        ));
    }

    #[test]
    fn a_zero_crossfade_cuts_straight_to_the_next_image() {
        let pictures = Pictures::new("cut", 2);
        let start = Instant::now();
        let mut engine = Engine::new(pictures.background(10, 0), render(), start);
        engine.poll(start + Duration::from_secs(10));
        assert!(matches!(
            engine.frame(start + Duration::from_secs(10)),
            Frame::Image { .. }
        ));
    }

    #[test]
    fn a_slideshow_can_be_advanced_by_hand_in_both_directions() {
        let pictures = Pictures::new("manual", 3);
        let now = Instant::now();
        let mut engine = Engine::new(pictures.background(3600, 0), render(), now);

        assert!(engine.advance(1, now));
        assert_eq!(
            engine.current_image().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("01.png"))
        );
        assert!(engine.advance(-1, now));
        assert_eq!(
            engine.current_image().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("00.png"))
        );
    }

    /// Advancing by hand must reset the interval, or the next automatic
    /// advance arrives immediately afterwards and the manual step is undone.
    #[test]
    fn advancing_by_hand_restarts_the_interval() {
        let pictures = Pictures::new("restart", 3);
        let start = Instant::now();
        let mut engine = Engine::new(pictures.background(60, 0), render(), start);

        engine.advance(1, start + Duration::from_secs(59));
        assert!(
            !engine.poll(start + Duration::from_secs(60)),
            "the interval did not restart"
        );
        assert!(engine.poll(start + Duration::from_secs(119)));
    }

    /// One corrupt file must not stall a slideshow.
    #[test]
    fn a_file_that_will_not_decode_is_skipped() {
        let pictures = Pictures::new("corrupt", 3);
        std::fs::write(pictures.0.join("01.png"), b"not actually a png").expect("write");

        let start = Instant::now();
        let mut engine = Engine::new(pictures.background(10, 0), render(), start);
        assert_eq!(
            engine.current_image().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("00.png"))
        );

        engine.poll(start + Duration::from_secs(10));
        assert_eq!(
            engine.current_image().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("02.png")),
            "the corrupt file was not skipped"
        );
    }

    /// A directory where *nothing* decodes must leave the previous image up
    /// rather than blanking the screen.
    #[test]
    fn a_directory_of_broken_files_keeps_what_was_on_screen() {
        let pictures = Pictures::new("all-broken", 2);
        let start = Instant::now();
        let mut engine = Engine::new(pictures.background(10, 0), render(), start);
        let showing = engine.current_image().map(Path::to_path_buf);
        assert!(showing.is_some());

        for index in 0..2 {
            std::fs::write(pictures.0.join(format!("{index:02}.png")), b"broken").expect("write");
        }
        engine.poll(start + Duration::from_secs(10));

        assert_eq!(engine.current_image().map(Path::to_path_buf), showing);
        assert!(matches!(engine.frame(start), Frame::Image { .. }));
    }

    #[test]
    fn a_rescan_picks_up_a_directory_that_has_been_filled() {
        let pictures = Pictures::new("filled", 0);
        let now = Instant::now();
        let mut engine = Engine::new(pictures.background(60, 0), render(), now);
        assert_eq!(engine.playlist_len(), 0);
        assert!(matches!(engine.frame(now), Frame::Color(_)));

        std::fs::write(pictures.0.join("new.png"), png(200)).expect("write");
        assert!(engine.rescan_directory(), "the new file was not noticed");
        assert!(matches!(engine.frame(now), Frame::Image { .. }));
    }

    #[test]
    fn every_image_gets_its_own_identifier() {
        let pictures = Pictures::new("ids", 3);
        let start = Instant::now();
        let mut engine = Engine::new(pictures.background(10, 0), render(), start);

        let mut ids = Vec::new();
        for step in 0..3 {
            let Frame::Image { id, .. } = engine.frame(start) else {
                panic!()
            };
            ids.push(id);
            engine.advance(1, start + Duration::from_secs(step));
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3, "two images shared a cache key");
    }
}
