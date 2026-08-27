//! Turning a [`Background`] into things that can be drawn.
//!
//! Everything crossing the control socket or arriving in a config file is a
//! string: `"#7AA2F7"`, `"cover"`, `"aurora"`. This is the single place they
//! become `Color`, `Fit` and `Scene`, which is what the protocol crate's
//! documentation promises -- one parser, one set of error messages, whether
//! the value was typed into a file or into a terminal.
//!
//! The errors are written to be printed at somebody. `"#GGGGGG" is not a
//! colour; write #RGB, #RRGGBB or #AARRGGBB` is what the CLI prints and what
//! the log line says, because it is the same string.

use anyhow::{Context, Result};
use raven_canvas_proto::Background;
use raven_paint::{Color, Fit};
use raven_scene::{Kind, Palette, Scene};

use std::time::Duration;

/// A background with everything parsed.
///
/// The image *files* are not here -- those are loaded by the engine, which
/// owns the decoded pixels and the slideshow position. This is only the
/// settings.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Plan {
    /// One flat colour.
    Color(Color),
    /// One file, named by the engine.
    Image { fit: Fit, background: Color },
    /// A directory of files.
    Slideshow {
        fit: Fit,
        background: Color,
        interval: Duration,
        crossfade: Duration,
        shuffle: bool,
    },
    /// Something computed.
    Scene(Box<Scene>),
}

impl Plan {
    /// Whether this plan expects to change on its own.
    ///
    /// A slideshow counts even though it is still for minutes at a time: the
    /// question this answers is "does the daemon need a timer", and it does.
    pub(crate) fn is_animated(&self) -> bool {
        match self {
            Self::Color(_) | Self::Image { .. } => false,
            Self::Slideshow { .. } => true,
            Self::Scene(scene) => scene.is_animated(),
        }
    }
}

/// Parse a [`Background`].
pub(crate) fn plan(background: &Background) -> Result<Plan> {
    Ok(match background {
        Background::Color { color } => Plan::Color(colour(color)?),
        Background::Image {
            fit, background, ..
        } => Plan::Image {
            fit: fitting(fit)?,
            background: colour(background)?,
        },
        Background::Slideshow {
            interval,
            shuffle,
            crossfade,
            fit,
            background,
            ..
        } => Plan::Slideshow {
            fit: fitting(fit)?,
            background: colour(background)?,
            // A slideshow that changes every second is a strobe, and one whose
            // interval is zero would advance every time the loop woke. Both
            // are clamped rather than refused -- somebody typing `interval = 0`
            // meant "fast", not "break".
            interval: Duration::from_secs((*interval).clamp(1, 86_400)),
            crossfade: Duration::from_millis((*crossfade).min(10_000)),
            shuffle: *shuffle,
        },
        Background::Scene {
            name,
            speed,
            palette,
        } => {
            let kind: Kind = name.parse().map_err(anyhow::Error::new)?;
            let stops = if palette.is_empty() {
                kind.default_palette()
            } else {
                let colours = palette
                    .iter()
                    .map(|stop| colour(stop))
                    .collect::<Result<Vec<Color>>>()
                    .context("in the scene's palette")?;
                Palette::new(colours).map_err(anyhow::Error::new)?
            };
            Plan::Scene(Box::new(Scene::new(kind, stops, *speed)))
        }
    })
}

fn colour(text: &str) -> Result<Color> {
    text.parse().map_err(anyhow::Error::new)
}

fn fitting(text: &str) -> Result<Fit> {
    text.parse().map_err(anyhow::Error::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_resolves() {
        let plan = plan(&Background::Color {
            color: "#7AA2F7".into(),
        })
        .unwrap();
        assert_eq!(plan, Plan::Color(Color::from_argb(0xFF7A_A2F7)));
        assert!(!plan.is_animated());
    }

    #[test]
    fn an_image_resolves_its_fit_and_letterbox_colour() {
        let plan = plan(&Background::Image {
            path: "/a.png".into(),
            fit: "contain".into(),
            background: "#000".into(),
        })
        .unwrap();
        assert_eq!(
            plan,
            Plan::Image {
                fit: Fit::Contain,
                background: Color::from_argb(0xFF00_0000),
            }
        );
        assert!(!plan.is_animated(), "a still image needs no timer");
    }

    #[test]
    fn a_scene_with_no_palette_gets_the_scenes_own() {
        let Plan::Scene(scene) = plan(&Background::Scene {
            name: "aurora".into(),
            speed: 1.0,
            palette: Vec::new(),
        })
        .unwrap() else {
            panic!("not a scene");
        };
        assert_eq!(scene.kind(), Kind::Aurora);
        assert_eq!(scene.palette(), &Kind::Aurora.default_palette());
    }

    #[test]
    fn a_scene_with_a_palette_uses_it() {
        let Plan::Scene(scene) = plan(&Background::Scene {
            name: "plasma".into(),
            speed: 2.0,
            palette: vec!["#000000".into(), "#FFFFFF".into()],
        })
        .unwrap() else {
            panic!("not a scene");
        };
        assert_eq!(scene.palette().len(), 2);
        assert_eq!(scene.speed(), 2.0);
    }

    #[test]
    fn a_scene_at_zero_speed_needs_no_timer() {
        let plan = plan(&Background::Scene {
            name: "gradient".into(),
            speed: 0.0,
            palette: Vec::new(),
        })
        .unwrap();
        assert!(
            !plan.is_animated(),
            "a frozen scene must not keep the daemon awake"
        );
    }

    #[test]
    fn a_slideshow_always_needs_a_timer() {
        let plan = plan(&Background::Slideshow {
            directory: "/pics".into(),
            interval: 60,
            shuffle: false,
            crossfade: 500,
            fit: "cover".into(),
            background: "#16161F".into(),
        })
        .unwrap();
        assert!(plan.is_animated());
    }

    /// `interval = 0` means "fast", not "spin". Clamping is the reading that
    /// does something rather than the one that breaks.
    #[test]
    fn an_absurd_slideshow_interval_is_clamped() {
        let make = |interval, crossfade| {
            plan(&Background::Slideshow {
                directory: "/pics".into(),
                interval,
                shuffle: false,
                crossfade,
                fit: "cover".into(),
                background: "#16161F".into(),
            })
            .unwrap()
        };

        let Plan::Slideshow {
            interval,
            crossfade,
            ..
        } = make(0, 0)
        else {
            panic!()
        };
        assert_eq!(interval, Duration::from_secs(1));
        assert_eq!(
            crossfade,
            Duration::ZERO,
            "zero crossfade is a cut, and legal"
        );

        let Plan::Slideshow {
            interval,
            crossfade,
            ..
        } = make(u64::MAX, u64::MAX)
        else {
            panic!()
        };
        assert_eq!(interval, Duration::from_secs(86_400));
        assert_eq!(crossfade, Duration::from_secs(10));
    }

    // -- errors, which are printed at people ---------------------------------

    #[test]
    fn a_bad_colour_says_what_a_colour_looks_like() {
        let error = plan(&Background::Color {
            color: "#GGGGGG".into(),
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("#RRGGBB"), "{text}");
    }

    #[test]
    fn a_bad_fit_names_the_ones_that_exist() {
        let error = plan(&Background::Image {
            path: "/a.png".into(),
            fit: "squish".into(),
            background: "#000".into(),
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("cover"), "{error:#}");
    }

    #[test]
    fn a_bad_scene_names_the_ones_that_exist() {
        let error = plan(&Background::Scene {
            name: "fireplace".into(),
            speed: 1.0,
            palette: Vec::new(),
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("starfield"), "{error:#}");
    }

    #[test]
    fn a_bad_palette_colour_says_it_was_in_the_palette() {
        let error = plan(&Background::Scene {
            name: "plasma".into(),
            speed: 1.0,
            palette: vec!["#000000".into(), "not a colour".into()],
        })
        .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("palette"), "{text}");
    }

    #[test]
    fn an_empty_palette_written_out_is_refused_rather_than_silently_defaulted() {
        // Distinct from the field being absent: `palette = []` is somebody
        // saying something, and it does not mean anything.
        //
        // The protocol's `Vec<String>` cannot tell the two apart, so an empty
        // list *is* "use the defaults" -- this test pins that reading down so
        // it is a decision rather than an accident.
        let plan = plan(&Background::Scene {
            name: "plasma".into(),
            speed: 1.0,
            palette: Vec::new(),
        });
        assert!(plan.is_ok(), "an empty palette means the scene's own");
    }
}
