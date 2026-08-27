//! `ravencanvas` -- change the wallpaper.
//!
//! One connection, one message, one reply, and out. There is no state here and
//! no configuration: everything this can do, [`ravencanvasd`] can be asked to
//! do over its control socket, and everything it prints is a string the daemon
//! wrote.
//!
//! That is why this links the protocol crate and nothing else -- no renderer,
//! no image decoder, no Wayland. A command run from a keybinding should start
//! instantly, and the way to make sure it does is to give it nothing to load.
//!
//! # Why validation happens on the far side
//!
//! `ravencanvas set scene fireplace` is refused by the *daemon*, not here, and
//! the message printed is the daemon's. If this parsed scene names too there
//! would be two lists to keep in step, and the version skew between a CLI and
//! a daemon updated at different times would show up as one of them rejecting
//! something the other was fine with.
//!
//! The one exception is [`detect`], which needs to know whether a word is a
//! scene name to decide what the user meant by it -- so it *asks the daemon*
//! rather than guessing. See that function.

#![forbid(unsafe_code)]

use std::io::{BufReader, BufWriter};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use raven_canvas_proto::{Background, Request, Response, SceneInfo, Status};

/// How long to wait for the daemon to answer.
///
/// Every request it serves is arithmetic and a file read. Anything slower than
/// this is a daemon in trouble, and a CLI that hangs on it is worse than one
/// that says so.
const TIMEOUT: Duration = Duration::from_secs(10);

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        // The daemon said no. Its message has already been printed; the exit
        // code is what a script reads.
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("ravencanvas: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> &'static str {
    "ravencanvas -- change the RavenLinux wallpaper

  ravencanvas set <THING> [OPTIONS]     what THING is, is worked out from it
  ravencanvas set image <PATH>          one picture
  ravencanvas set slideshow <DIR>       every picture in a directory
  ravencanvas set scene <NAME>          something drawn rather than decoded
  ravencanvas set color <COLOUR>        one flat colour

  ravencanvas status                    what is on screen
  ravencanvas scenes                    which scenes this build has
  ravencanvas next | prev               move a slideshow along
  ravencanvas pause | resume            stop or restart animation
  ravencanvas reload                    re-read canvas.toml

Options for `set`:
  -p, --persist          also write it to ~/.config/raven/canvas.toml
      --fit MODE         cover | contain | stretch | center | tile
      --background HEX   shown where the picture does not reach
      --interval SECS    how long each slide is shown
      --crossfade MS     how long to fade between them; 0 cuts
      --shuffle          visit a slideshow in a shuffled order
      --speed X          a scene's rate; 0 draws one frame and stops
      --palette A,B,C    a scene's colours

Colours are #RGB, #RRGGBB or #AARRGGBB."
}

fn run() -> Result<bool> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = argv.first().map(String::as_str) else {
        println!("{}", usage());
        return Ok(true);
    };

    let request = match command {
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            return Ok(true);
        }
        "-V" | "--version" => {
            println!("ravencanvas {}", env!("CARGO_PKG_VERSION"));
            return Ok(true);
        }
        "status" => Request::Status,
        "scenes" => Request::Scenes,
        "reload" => Request::Reload,
        "next" => Request::Advance { by: 1 },
        "prev" | "previous" => Request::Advance { by: -1 },
        "pause" => Request::SetPaused { paused: true },
        "resume" => Request::SetPaused { paused: false },
        "set" => build_set(&argv[1..])?,
        other => bail!("unknown command {other:?}\n\n{}", usage()),
    };

    let response = send(&request)?;
    Ok(report(&response))
}

// ---------------------------------------------------------------------------
// `set`
// ---------------------------------------------------------------------------

/// The options `set` accepts, before it is known which of them apply.
#[derive(Debug, Default, PartialEq)]
struct SetOptions {
    persist: bool,
    fit: Option<String>,
    background: Option<String>,
    interval: Option<u64>,
    crossfade: Option<u64>,
    shuffle: bool,
    speed: Option<f32>,
    palette: Vec<String>,
}

/// What kind of thing `set` was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Image,
    Slideshow,
    Scene,
    Color,
}

impl Kind {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "image" | "picture" => Some(Self::Image),
            "slideshow" | "directory" | "dir" => Some(Self::Slideshow),
            "scene" => Some(Self::Scene),
            "color" | "colour" => Some(Self::Color),
            _ => None,
        }
    }
}

fn build_set(args: &[String]) -> Result<Request> {
    let (positional, options) = split_options(args)?;
    let (kind, value) = match positional.as_slice() {
        [one] => (None, one.clone()),
        [first, second] => match Kind::parse(first) {
            Some(kind) => (Some(kind), second.clone()),
            None => bail!(
                "{first:?} is not one of image, slideshow, scene or color\n\n{}",
                usage()
            ),
        },
        [] => bail!("set needs something to show\n\n{}", usage()),
        _ => bail!("set takes one thing to show\n\n{}", usage()),
    };

    let kind = match kind {
        Some(kind) => kind,
        None => detect(&value)?,
    };

    Ok(Request::Apply {
        background: background_of(kind, &value, &options)?,
        persist: options.persist,
    })
}

/// Work out what the user meant by a bare `ravencanvas set <thing>`.
///
/// The order is deliberate, and the second step is the interesting one:
///
/// 1. **A colour**, because `#7AA2F7` cannot be anything else.
/// 2. **A scene**, *asked of the daemon* rather than matched against a list
///    compiled in here. That costs one extra round trip and it is worth it: a
///    daemon that has learned a new scene teaches this command about it with
///    no new release, and the two can never disagree about what exists.
/// 3. **A path**, a directory being a slideshow and a file being an image.
///
/// Scenes come before paths so that `ravencanvas set aurora` means the scene
/// even in a directory that happens to contain a file called `aurora`. The way
/// to ask for that file is `ravencanvas set image ./aurora`, which is what an
/// explicit form is for.
fn detect(value: &str) -> Result<Kind> {
    if looks_like_a_colour(value) {
        return Ok(Kind::Color);
    }

    // A daemon that cannot be reached is reported by `send` in a moment with a
    // better message than this could give, so a failure here falls through to
    // the path check rather than stopping.
    if let Ok(Response::Scenes { scenes }) = send(&Request::Scenes)
        && scenes.iter().any(|scene| scene.name == value)
    {
        return Ok(Kind::Scene);
    }

    let path = absolute(value)?;
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => Ok(Kind::Slideshow),
        Ok(_) => Ok(Kind::Image),
        Err(_) => bail!(
            "{value:?} is not a colour, a scene, or a path that exists\n\n\
             Try `ravencanvas scenes` for the scenes this build has."
        ),
    }
}

/// Whether a string can only be a colour.
///
/// Deliberately loose: anything starting with `#`, or a bare run of hex
/// digits of a plausible length. Getting this exactly right is the daemon's
/// job -- it has the parser -- and all this decides is which *kind* of thing
/// to send, so a malformed colour is better sent as a colour and refused with
/// a message about colours.
fn looks_like_a_colour(value: &str) -> bool {
    if value.starts_with('#') {
        return true;
    }
    matches!(value.len(), 3 | 6 | 8) && value.chars().all(|c| c.is_ascii_hexdigit())
}

fn background_of(kind: Kind, value: &str, options: &SetOptions) -> Result<Background> {
    // Defaults live in the protocol crate, and are spelled here by asking for
    // them rather than by repeating the numbers.
    let defaults = Background::Slideshow {
        directory: PathBuf::new(),
        interval: default_interval(),
        shuffle: false,
        crossfade: default_crossfade(),
        fit: "cover".to_string(),
        background: "#16161F".to_string(),
    };
    let Background::Slideshow {
        interval: default_interval,
        crossfade: default_crossfade,
        fit: default_fit,
        background: default_background,
        ..
    } = defaults
    else {
        unreachable!("just constructed as a slideshow")
    };

    let fit = options.fit.clone().unwrap_or(default_fit);
    let background = options.background.clone().unwrap_or(default_background);

    Ok(match kind {
        Kind::Color => Background::Color {
            color: value.to_string(),
        },
        Kind::Image => Background::Image {
            path: absolute(value)?,
            fit,
            background,
        },
        Kind::Slideshow => Background::Slideshow {
            directory: absolute(value)?,
            interval: options.interval.unwrap_or(default_interval),
            shuffle: options.shuffle,
            crossfade: options.crossfade.unwrap_or(default_crossfade),
            fit,
            background,
        },
        Kind::Scene => Background::Scene {
            name: value.to_string(),
            speed: options.speed.unwrap_or(1.0),
            palette: options.palette.clone(),
        },
    })
}

const fn default_interval() -> u64 {
    900
}

const fn default_crossfade() -> u64 {
    800
}

/// Split flags out of an argument list.
fn split_options(args: &[String]) -> Result<(Vec<String>, SetOptions)> {
    let mut positional = Vec::new();
    let mut options = SetOptions::default();
    let mut rest = args.iter();

    while let Some(argument) = rest.next() {
        let mut value = |flag: &str| -> Result<String> {
            rest.next()
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };

        match argument.as_str() {
            "-p" | "--persist" => options.persist = true,
            "--shuffle" => options.shuffle = true,
            "--fit" => options.fit = Some(value("--fit")?),
            "--background" | "--bg" => options.background = Some(value("--background")?),
            "--interval" => {
                let text = value("--interval")?;
                options.interval = Some(
                    text.parse()
                        .with_context(|| format!("--interval takes seconds, not {text:?}"))?,
                );
            }
            "--crossfade" => {
                let text = value("--crossfade")?;
                options.crossfade =
                    Some(text.parse().with_context(|| {
                        format!("--crossfade takes milliseconds, not {text:?}")
                    })?);
            }
            "--speed" => {
                let text = value("--speed")?;
                options.speed = Some(
                    text.parse()
                        .with_context(|| format!("--speed takes a number, not {text:?}"))?,
                );
            }
            "--palette" => {
                options.palette = value("--palette")?
                    .split(',')
                    .map(|stop| stop.trim().to_string())
                    .filter(|stop| !stop.is_empty())
                    .collect();
            }
            other if other.starts_with('-') && other.len() > 1 => {
                bail!("unknown option {other:?}\n\n{}", usage())
            }
            other => positional.push(other.to_string()),
        }
    }
    Ok((positional, options))
}

/// Make a path absolute, so the daemon resolves it the same way the user meant
/// it.
///
/// A relative path sent as typed would be resolved against the *daemon's*
/// working directory, which is wherever the session started -- almost never
/// where the user is standing. `~` is expanded because it arrives unexpanded
/// often enough: from a quoted argument, from a script, from a keybinding.
///
/// Symlinks are deliberately not resolved. `~/Pictures/current.png` being a
/// symlink somebody re-points is a perfectly good way to change wallpaper, and
/// canonicalizing would pin this to whatever it pointed at today.
fn absolute(value: &str) -> Result<PathBuf> {
    let expanded = match value.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => Path::new(&home).join(rest),
            None => bail!("cannot expand {value:?}: there is no $HOME"),
        },
        None if value == "~" => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home),
            None => bail!("cannot expand {value:?}: there is no $HOME"),
        },
        None => PathBuf::from(value),
    };

    if expanded.is_absolute() {
        return Ok(expanded);
    }
    let current = std::env::current_dir().context("cannot find the current directory")?;
    Ok(current.join(expanded))
}

// ---------------------------------------------------------------------------
// Talking to the daemon
// ---------------------------------------------------------------------------

fn send(request: &Request) -> Result<Response> {
    let path = raven_canvas_proto::socket_path().context(
        "there is no $XDG_RUNTIME_DIR, so there is nowhere for a control socket; is this a session?",
    )?;

    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "cannot reach ravencanvasd at {}; is it running?",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();

    let mut writer = BufWriter::new(stream.try_clone().context("cannot clone the socket")?);
    raven_canvas_proto::write_message(&mut writer, request).context("cannot send the request")?;

    let mut reader = BufReader::new(stream);
    raven_canvas_proto::read_message(&mut reader).context("cannot read the reply")
}

/// Print a reply. Returns whether it was a success.
fn report(response: &Response) -> bool {
    match response {
        Response::Ok { message } => {
            println!("{message}");
            true
        }
        Response::Failed { message } => {
            eprintln!("ravencanvas: {message}");
            false
        }
        Response::Status(status) => {
            print!("{}", format_status(status));
            true
        }
        Response::Scenes { scenes } => {
            print!("{}", format_scenes(scenes));
            true
        }
    }
}

fn format_status(status: &Status) -> String {
    let mut out = String::new();
    let mut row = |label: &str, value: String| {
        out.push_str(&format!("{label:<12}{value}\n"));
    };

    row("background", status.background.describe());
    if let Some(image) = &status.current_image {
        row("showing", image.display().to_string());
    }
    if status.playlist > 0 {
        row("playlist", format!("{} images", status.playlist));
    }
    row(
        "config",
        match &status.config_path {
            Some(path) => path.display().to_string(),
            None => "none; using the defaults".to_string(),
        },
    );
    row(
        "state",
        format!(
            "{}, {}",
            if status.paused { "paused" } else { "running" },
            if status.animated { "animated" } else { "still" }
        ),
    );
    row("frames", status.frames.to_string());

    if status.outputs.is_empty() {
        row("outputs", "none".to_string());
    } else {
        for (index, output) in status.outputs.iter().enumerate() {
            row(
                if index == 0 { "outputs" } else { "" },
                format!(
                    "{}  {}x{}{}",
                    output.name,
                    output.width,
                    output.height,
                    if output.scale > 1 {
                        format!(" @{}x", output.scale)
                    } else {
                        String::new()
                    }
                ),
            );
        }
    }
    out
}

fn format_scenes(scenes: &[SceneInfo]) -> String {
    let width = scenes.iter().map(|s| s.name.len()).max().unwrap_or(0);
    scenes
        .iter()
        .map(|scene| format!("{:<width$}  {}\n", scene.name, scene.summary))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use raven_canvas_proto::OutputStatus;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn set(list: &[&str]) -> Background {
        match build_set(&args(list)).expect("build") {
            Request::Apply { background, .. } => background,
            other => panic!("expected an apply, got {other:?}"),
        }
    }

    // -- what kind of thing was named ---------------------------------------

    #[test]
    fn every_explicit_kind_is_recognised() {
        assert_eq!(Kind::parse("image"), Some(Kind::Image));
        assert_eq!(Kind::parse("picture"), Some(Kind::Image));
        assert_eq!(Kind::parse("slideshow"), Some(Kind::Slideshow));
        assert_eq!(Kind::parse("dir"), Some(Kind::Slideshow));
        assert_eq!(Kind::parse("scene"), Some(Kind::Scene));
        assert_eq!(Kind::parse("color"), Some(Kind::Color));
        assert_eq!(Kind::parse("colour"), Some(Kind::Color));
        assert_eq!(Kind::parse("video"), None);
    }

    #[test]
    fn colours_are_recognised_by_shape() {
        for value in ["#7AA2F7", "#7AF", "#807AA2F7", "16161F", "7AF"] {
            assert!(looks_like_a_colour(value), "{value}");
        }
    }

    /// Loose, but not so loose that a filename becomes a colour. `abcdef` is
    /// the awkward case and is deliberately taken as a colour -- a file with
    /// that name and no extension is rarer than somebody typing a bare hex
    /// triple, and `set image abcdef` says the other thing.
    #[test]
    fn things_that_are_not_colours_are_not() {
        for value in ["aurora", "/usr/share/backgrounds", "photo.png", "12345", ""] {
            assert!(!looks_like_a_colour(value), "{value}");
        }
    }

    // -- building a request -------------------------------------------------

    #[test]
    fn an_explicit_colour_becomes_a_colour_background() {
        assert_eq!(
            set(&["color", "#7AA2F7"]),
            Background::Color {
                color: "#7AA2F7".into()
            }
        );
    }

    #[test]
    fn an_image_gets_the_stock_fit_and_letterbox_colour() {
        let Background::Image {
            fit, background, ..
        } = set(&["image", "/tmp/a.png"])
        else {
            panic!("not an image");
        };
        assert_eq!(fit, "cover");
        assert_eq!(background, "#16161F");
    }

    #[test]
    fn an_images_options_are_carried_through() {
        let Background::Image {
            fit,
            background,
            path,
        } = set(&[
            "image",
            "/tmp/a.png",
            "--fit",
            "contain",
            "--background",
            "#000",
        ])
        else {
            panic!("not an image");
        };
        assert_eq!((fit.as_str(), background.as_str()), ("contain", "#000"));
        assert_eq!(path, PathBuf::from("/tmp/a.png"));
    }

    #[test]
    fn a_slideshow_gets_the_stock_interval_and_crossfade() {
        let Background::Slideshow {
            interval,
            crossfade,
            shuffle,
            ..
        } = set(&["slideshow", "/tmp/pics"])
        else {
            panic!("not a slideshow");
        };
        assert_eq!((interval, crossfade, shuffle), (900, 800, false));
    }

    #[test]
    fn a_slideshows_options_are_carried_through() {
        let Background::Slideshow {
            interval,
            crossfade,
            shuffle,
            ..
        } = set(&[
            "slideshow",
            "/tmp/pics",
            "--interval",
            "60",
            "--crossfade",
            "0",
            "--shuffle",
        ])
        else {
            panic!("not a slideshow");
        };
        assert_eq!((interval, crossfade, shuffle), (60, 0, true));
    }

    #[test]
    fn a_scenes_palette_is_split_on_commas_and_trimmed() {
        let Background::Scene {
            name,
            speed,
            palette,
        } = set(&[
            "scene",
            "aurora",
            "--speed",
            "0.5",
            "--palette",
            "#000000, #7AA2F7 ,#FFFFFF,",
        ])
        else {
            panic!("not a scene");
        };
        assert_eq!(name, "aurora");
        assert_eq!(speed, 0.5);
        assert_eq!(palette, vec!["#000000", "#7AA2F7", "#FFFFFF"]);
    }

    #[test]
    fn persist_is_off_unless_asked_for() {
        let Request::Apply { persist, .. } = build_set(&args(&["color", "#000"])).unwrap() else {
            panic!()
        };
        assert!(!persist);

        for flag in ["-p", "--persist"] {
            let Request::Apply { persist, .. } =
                build_set(&args(&["color", "#000", flag])).unwrap()
            else {
                panic!()
            };
            assert!(persist, "{flag}");
        }
    }

    // -- argument errors, which are printed at people ------------------------

    #[test]
    fn set_with_nothing_says_what_it_wants() {
        let error = build_set(&[]).unwrap_err().to_string();
        assert!(error.contains("something to show"), "{error}");
    }

    #[test]
    fn an_unknown_kind_lists_the_ones_that_exist() {
        let error = build_set(&args(&["video", "/tmp/a.mp4"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("slideshow"), "{error}");
    }

    #[test]
    fn an_option_missing_its_value_says_so() {
        let error = split_options(&args(&["--fit"])).unwrap_err().to_string();
        assert!(error.contains("--fit needs a value"), "{error}");
    }

    #[test]
    fn a_non_numeric_interval_is_an_error_naming_what_was_typed() {
        let error = split_options(&args(&["--interval", "soon"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("soon"), "{error}");
    }

    #[test]
    fn an_unknown_option_is_refused_rather_than_taken_as_a_path() {
        assert!(split_options(&args(&["--loop"])).is_err());
    }

    /// A negative number is a value, not a flag. `--speed -1` has to work.
    #[test]
    fn a_negative_number_is_not_mistaken_for_an_option() {
        let (_, options) = split_options(&args(&["--speed", "-1.5"])).unwrap();
        assert_eq!(options.speed, Some(-1.5));
    }

    // -- paths ---------------------------------------------------------------

    #[test]
    fn an_absolute_path_is_left_alone() {
        assert_eq!(absolute("/tmp/a.png").unwrap(), PathBuf::from("/tmp/a.png"));
    }

    /// The daemon's working directory is wherever the session started, so a
    /// relative path sent as typed would resolve somewhere the user has never
    /// been.
    #[test]
    fn a_relative_path_is_made_absolute() {
        let resolved = absolute("wallpaper.png").unwrap();
        assert!(resolved.is_absolute(), "{resolved:?}");
        assert!(resolved.ends_with("wallpaper.png"));
    }

    #[test]
    fn a_tilde_is_expanded_when_there_is_a_home() {
        let Some(home) = std::env::var_os("HOME") else {
            return; // Nothing to test against on a machine with no $HOME.
        };
        assert_eq!(
            absolute("~/Pictures/a.png").unwrap(),
            Path::new(&home).join("Pictures/a.png")
        );
        assert_eq!(absolute("~").unwrap(), PathBuf::from(home));
    }

    // -- printing ------------------------------------------------------------

    fn status() -> Status {
        Status {
            background: Background::Scene {
                name: "aurora".into(),
                speed: 1.0,
                palette: vec![],
            },
            config_path: Some("/home/a/.config/raven/canvas.toml".into()),
            current_image: None,
            playlist: 0,
            paused: false,
            animated: true,
            frames: 4321,
            outputs: vec![OutputStatus {
                name: "eDP-1".into(),
                width: 2560,
                height: 1600,
                scale: 2,
            }],
        }
    }

    #[test]
    fn status_prints_everything_worth_knowing() {
        let text = format_status(&status());
        assert!(text.contains("scene aurora"), "{text}");
        assert!(text.contains("canvas.toml"), "{text}");
        assert!(text.contains("running, animated"), "{text}");
        assert!(text.contains("4321"), "{text}");
        assert!(text.contains("eDP-1  2560x1600 @2x"), "{text}");
    }

    /// A slideshow's rows only appear when there is a slideshow. A `playlist`
    /// row reading `0 images` under a scene would be noise.
    #[test]
    fn status_leaves_out_the_rows_that_do_not_apply() {
        let text = format_status(&status());
        assert!(!text.contains("playlist"), "{text}");
        assert!(!text.contains("showing"), "{text}");
    }

    #[test]
    fn status_shows_a_slideshows_rows_when_there_is_one() {
        let mut status = status();
        status.playlist = 12;
        status.current_image = Some("/pics/03.png".into());
        status.paused = true;

        let text = format_status(&status);
        assert!(text.contains("12 images"), "{text}");
        assert!(text.contains("/pics/03.png"), "{text}");
        assert!(text.contains("paused"), "{text}");
    }

    #[test]
    fn status_says_so_when_there_is_no_config_file() {
        let mut status = status();
        status.config_path = None;
        assert!(format_status(&status).contains("using the defaults"));
    }

    #[test]
    fn status_with_no_outputs_says_none_rather_than_printing_nothing() {
        let mut status = status();
        status.outputs.clear();
        assert!(format_status(&status).contains("none"));
    }

    #[test]
    fn scenes_are_printed_in_aligned_columns() {
        let scenes = vec![
            SceneInfo {
                name: "plasma".into(),
                summary: "waves".into(),
            },
            SceneInfo {
                name: "starfield".into(),
                summary: "stars".into(),
            },
        ];
        let text = format_scenes(&scenes);
        let columns: Vec<usize> = text
            .lines()
            .zip(&scenes)
            .map(|(line, scene)| line.rfind(&scene.summary).expect("the summary"))
            .collect();
        assert_eq!(
            columns[0], columns[1],
            "the summaries are not aligned:\n{text}"
        );
    }

    #[test]
    fn printing_a_failure_reports_it_as_one() {
        assert!(!report(&Response::Failed {
            message: "no".into()
        }));
        assert!(report(&Response::Ok {
            message: "yes".into()
        }));
    }
}
