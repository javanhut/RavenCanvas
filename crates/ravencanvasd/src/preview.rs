//! `--preview`: one frame, to a PNG, with no compositor.
//!
//! This is how the wallpaper is iterated on. The alternative is rebooting a
//! machine, or at best restarting a session, to find out what a palette looks
//! like -- which is enough friction that nobody tunes anything and the scenes
//! stay however they were first written.
//!
//! It renders through [`crate::screen::paint`], the same function the real
//! surface uses, rather than through a second implementation. A preview whose
//! output is not exactly what the compositor would show is worse than no
//! preview, because it is believed.
//!
//! The one thing it cannot show is a *change over time*, which is why the time
//! argument exists: `--preview out.png 1920x1080 300` draws the frame five
//! minutes into the loop. Compare two of those and the animation's range is
//! visible without watching it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use raven_canvas_proto::Background;
use raven_paint::{Canvas, Field};

use crate::config;
use crate::engine::Engine;
use crate::screen::{self, FitCache};

/// What `--preview` was asked for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Options {
    pub output: PathBuf,
    pub width: i32,
    pub height: i32,
    /// Seconds into the animation loop.
    pub seconds: f64,
    /// A scene name overriding whatever is configured.
    pub scene: Option<String>,
}

/// Parse `--preview`'s arguments.
///
/// `<file> [WIDTHxHEIGHT] [SECONDS] [SCENE]`, all optional after the first.
/// Positional rather than flagged because this is a development command with
/// three arguments, and `--preview out.png 1920x1080 300 aurora` reads better
/// than the same thing with four flag names in it.
pub(crate) fn parse(args: &[String]) -> Result<Options> {
    let Some(output) = args.first() else {
        bail!("--preview needs a file to write: --preview out.png [WxH] [SECONDS] [SCENE]");
    };

    let mut options = Options {
        output: PathBuf::from(output),
        width: 1920,
        height: 1080,
        seconds: 0.0,
        scene: None,
    };

    for argument in &args[1..] {
        if let Some((width, height)) = argument.split_once(['x', 'X']) {
            options.width = width
                .parse()
                .with_context(|| format!("{argument:?} is not a size like 1920x1080"))?;
            options.height = height
                .parse()
                .with_context(|| format!("{argument:?} is not a size like 1920x1080"))?;
        } else if let Ok(seconds) = argument.parse::<f64>() {
            options.seconds = seconds;
        } else {
            options.scene = Some(argument.clone());
        }
    }

    if options.width <= 0 || options.height <= 0 {
        bail!("a preview cannot be {}x{}", options.width, options.height);
    }
    // The same ceiling the decoder uses, for the same reason: a typo in a
    // size should be an error rather than an allocation.
    if (options.width as u64) * (options.height as u64) > raven_paint::MAX_PIXELS as u64 {
        bail!(
            "{}x{} is past the {}-pixel limit",
            options.width,
            options.height,
            raven_paint::MAX_PIXELS
        );
    }
    Ok(options)
}

/// Render one frame and write it.
pub(crate) fn run(options: &Options, config_paths: &[PathBuf]) -> Result<()> {
    let loaded = config::load_from(config_paths, |path, error| {
        eprintln!("ravencanvasd: ignoring {}: {error:#}", path.display());
    });

    let background = match &options.scene {
        Some(name) => Background::Scene {
            name: name.clone(),
            speed: 1.0,
            palette: Vec::new(),
        },
        None => loaded.background(),
    };

    // `Instant` is monotonic and cannot be constructed at an arbitrary point,
    // so the requested time is expressed as a start in the past. Subtracting
    // rather than adding keeps it in range on a machine that has just booted.
    let now = Instant::now();
    let started = now
        .checked_sub(Duration::from_secs_f64(options.seconds.max(0.0)))
        .unwrap_or(now);

    let engine = Engine::new(background, loaded.config.render, started);
    let frame = engine.frame(now);

    let mut data = vec![
        0u8;
        raven_paint::buffer_len(options.width, options.height)
            .context("that preview size does not fit in memory")?
    ];
    let mut field = Field::new(1, 1);
    let mut cache = FitCache::default();

    screen::paint(
        &mut Canvas::new(&mut data, options.width, options.height),
        &frame,
        &mut field,
        &mut cache,
        loaded.config.render.detail,
    );

    write_png(&options.output, &data, options.width, options.height)?;
    eprintln!(
        "ravencanvasd: wrote {} ({}x{}, {}s into {})",
        options.output.display(),
        options.width,
        options.height,
        options.seconds,
        engine.background().describe()
    );
    Ok(())
}

/// Write a canvas buffer out as an opaque PNG.
///
/// The canvas is `[B, G, R, A]` and the encoder wants `[R, G, B]`; alpha is
/// dropped rather than written, because everything this daemon draws is
/// opaque and an RGBA PNG would be a third larger for a channel of `0xFF`.
fn write_png(path: &Path, data: &[u8], width: i32, height: i32) -> Result<()> {
    let mut rgb = Vec::with_capacity(data.len() / 4 * 3);
    for pixel in data.chunks_exact(4) {
        rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }

    let file =
        std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);

    let mut writer = encoder
        .write_header()
        .with_context(|| format!("cannot write a PNG header to {}", path.display()))?;
    writer
        .write_image_data(&rgb)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_file_is_the_only_required_argument() {
        let options = parse(&args(["out.png"].as_slice())).unwrap();
        assert_eq!(options.output, PathBuf::from("out.png"));
        assert_eq!((options.width, options.height), (1920, 1080));
        assert_eq!(options.seconds, 0.0);
        assert_eq!(options.scene, None);
    }

    #[test]
    fn nothing_at_all_is_an_error_that_says_what_to_type() {
        let error = parse(&[]).unwrap_err().to_string();
        assert!(error.contains("--preview out.png"), "{error}");
    }

    #[test]
    fn the_optional_arguments_are_recognised_by_shape() {
        let options = parse(&args(&["out.png", "800x600", "12.5", "aurora"])).unwrap();
        assert_eq!((options.width, options.height), (800, 600));
        assert_eq!(options.seconds, 12.5);
        assert_eq!(options.scene.as_deref(), Some("aurora"));
    }

    /// Recognised by shape, so they may be written in any order. This is what
    /// makes the positional form tolerable.
    #[test]
    fn the_optional_arguments_may_be_in_any_order() {
        let a = parse(&args(&["out.png", "plasma", "300", "640x480"])).unwrap();
        let b = parse(&args(&["out.png", "640x480", "300", "plasma"])).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn a_capital_x_is_also_a_size() {
        assert_eq!(parse(&args(&["out.png", "640X480"])).unwrap().width, 640);
    }

    #[test]
    fn a_malformed_size_is_an_error_rather_than_a_scene_name() {
        let error = parse(&args(&["out.png", "1920xyz"])).unwrap_err();
        assert!(format!("{error:#}").contains("1920xyz"), "{error:#}");
    }

    #[test]
    fn an_impossible_size_is_refused() {
        assert!(parse(&args(&["out.png", "0x100"])).is_err());
        assert!(parse(&args(&["out.png", "-4x100"])).is_err());
        assert!(
            parse(&args(&["out.png", "60000x60000"])).is_err(),
            "past the pixel limit"
        );
    }

    /// The whole command, end to end: it must produce a PNG that decodes back
    /// to the size that was asked for.
    #[test]
    fn a_preview_writes_a_png_that_can_be_read_back() {
        let path = std::env::temp_dir().join("ravencanvas-preview-test.png");
        let _ = std::fs::remove_file(&path);

        let options = Options {
            output: path.clone(),
            width: 64,
            height: 40,
            seconds: 30.0,
            scene: Some("aurora".to_string()),
        };
        // No config paths: the scene override is the whole input, which is
        // what makes this test independent of the machine it runs on.
        run(&options, &[]).expect("preview");

        let bytes = std::fs::read(&path).expect("read back");
        let image = raven_paint::Image::decode(&bytes).expect("decode");
        assert_eq!((image.width(), image.height()), (64, 40));
        assert!(image.is_opaque());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn two_times_in_the_loop_produce_different_frames() {
        let directory = std::env::temp_dir();
        let early = directory.join("ravencanvas-preview-early.png");
        let late = directory.join("ravencanvas-preview-late.png");

        for (path, seconds) in [(&early, 0.0), (&late, raven_scene::LOOP_SECONDS / 4.0)] {
            run(
                &Options {
                    output: path.clone(),
                    width: 48,
                    height: 32,
                    seconds,
                    scene: Some("plasma".to_string()),
                },
                &[],
            )
            .expect("preview");
        }

        let a = std::fs::read(&early).expect("read");
        let b = std::fs::read(&late).expect("read");
        assert_ne!(a, b, "the scene did not move over a quarter of its loop");

        let _ = std::fs::remove_file(&early);
        let _ = std::fs::remove_file(&late);
    }

    #[test]
    fn an_unwritable_destination_is_an_error_naming_it() {
        let options = Options {
            output: PathBuf::from("/nonexistent/directory/out.png"),
            width: 8,
            height: 8,
            seconds: 0.0,
            scene: Some("gradient".to_string()),
        };
        let error = run(&options, &[]).unwrap_err();
        assert!(format!("{error:#}").contains("out.png"), "{error:#}");
    }

    #[test]
    fn a_scene_that_does_not_exist_falls_back_rather_than_failing() {
        // `Engine::new` cannot fail: a background that does not resolve is
        // reported and replaced by the fallback colour. A preview of one is
        // therefore a flat image, not an error.
        let path = std::env::temp_dir().join("ravencanvas-preview-bogus.png");
        run(
            &Options {
                output: path.clone(),
                width: 16,
                height: 16,
                seconds: 0.0,
                scene: Some("fireplace".to_string()),
            },
            &[],
        )
        .expect("preview");

        let bytes = std::fs::read(&path).expect("read back");
        assert!(raven_paint::Image::decode(&bytes).is_ok());
        let _ = std::fs::remove_file(&path);
    }
}
