//! `canvas.toml`.
//!
//! # Why there is a config file at all
//!
//! huginn ships one look and no configuration, and `docs/integration.md` says
//! plainly that there will not be one. That is right for a compositor: a theme
//! schema is a thing that drifts under people between releases.
//!
//! This file is not a theme. It holds one fact -- *which picture the user
//! wants on their screen* -- and there is no way to express that in a
//! protocol, a constant, or a chord. It is the same argument `ravend`'s
//! `login.toml` makes for holding the greeter's account name.
//!
//! # Two files, and the user's wins outright
//!
//! `/etc/raven/canvas.toml` is what an image ships. `~/.config/raven/canvas.toml`
//! is what a person writes. If the second exists, the first is not read at all.
//!
//! That is a deliberate choice against field-by-field merging, which is what
//! most desktops do and what looks more flexible. Merging cannot work here:
//! `[background]` is a tagged union, and half a `mode = "slideshow"` merged
//! over a `mode = "image"` is not a background, it is a puzzle. "Your file
//! wins" is a rule that can be explained in one sentence and predicted without
//! reading the code.
//!
//! # A broken file is not a fatal error
//!
//! `ravend` refuses to start on a `login.toml` it cannot parse, because
//! falling back would quietly ignore a policy somebody wrote down and
//! believed. The opposite is right here. A wallpaper is not a policy, this
//! daemon reloads on every edit, and the moment a user is most likely to have
//! a broken file is halfway through editing one. So a bad file logs a warning
//! and leaves what is already on screen; it never blanks the desktop and never
//! takes the process down.
//!
//! # A file that says nothing about the background says nothing about it
//!
//! `[background]` is optional and being absent is not the same as being the
//! default, which is why it is an `Option` here rather than a `Background`
//! with a `Default`. A `canvas.toml` holding only `[render]` is somebody
//! setting a frame rate, not somebody choosing the built-in gradient, and
//! [`Loaded::background`] fills the gap with the wallpaper this machine has
//! set before it falls back to the built-in. See [`crate::installed`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use raven_canvas_proto::Background;
use serde::{Deserialize, Serialize};

use crate::installed;

/// The system-wide file, shipped by `scripts/install.sh`.
pub(crate) const SYSTEM_PATH: &str = "/etc/raven/canvas.toml";

/// The user's file, relative to their config directory.
pub(crate) const USER_RELATIVE: &str = "raven/canvas.toml";

/// Everything `canvas.toml` can say.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Config {
    /// What to draw, or `None` when the file did not say.
    ///
    /// `skip_serializing_if` so that `config::save` of a config with no
    /// background writes a file with no `[background]` table, rather than
    /// failing -- TOML has no way to write a null. Nothing reaches `save`
    /// without one today (`--persist` always names what it is persisting),
    /// but a `Config` that cannot be written is a trap for the next caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<Background>,
    pub render: Render,
}

/// How hard to work at drawing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Render {
    /// Frames a second for an animated background.
    ///
    /// Thirty, not sixty. This is a wallpaper: it is behind everything, it is
    /// mostly covered, and nothing in it moves fast enough for the difference
    /// to be visible. Halving the rate halves what it costs, and that is the
    /// entire trade.
    pub fps: u32,
    /// The long edge of the buffer a scene is drawn into before it is
    /// upscaled, or zero for the scene's own choice.
    ///
    /// This is the dial to turn when a scene costs too much on a particular
    /// machine, and it is quadratic: halving it quarters the work. See
    /// `raven_paint::Field` for why drawing small and upscaling is not a
    /// compromise for the scenes this ships.
    pub detail: u32,
}

impl Default for Render {
    fn default() -> Self {
        Self { fps: 30, detail: 0 }
    }
}

impl Render {
    /// The gap between frames.
    ///
    /// Clamped to a range with defensible ends: below one frame a second there
    /// is no animation worth the name, and above 240 the daemon would be
    /// asking for frames faster than any panel can show them.
    pub(crate) fn frame_interval(self) -> std::time::Duration {
        let fps = self.fps.clamp(1, 240);
        std::time::Duration::from_nanos(1_000_000_000 / u64::from(fps))
    }
}

/// Where the config was read from, and what it said.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Loaded {
    pub config: Config,
    /// `None` when neither file exists, which is a supported state: the
    /// defaults are a perfectly good desktop background.
    pub path: Option<PathBuf>,
}

impl Loaded {
    /// The background to draw, from the first of three sources that has one.
    ///
    /// 1. What a config file named. A person's own file beats the system one,
    ///    and both beat everything below.
    /// 2. The wallpaper this machine has set, at
    ///    `/usr/share/wallpaper/set/wallpaper.*` -- the same file RavenLogin's
    ///    greeter draws the login screen on when `login.toml` names none of
    ///    its own. See [`crate::installed`] for why that path is a contract
    ///    rather than a preference.
    /// 3. The built-in, which is a gradient and needs nothing on disk.
    ///
    /// This touches the filesystem, so it is a method rather than a field:
    /// calling it again after `set/` has changed gives the new answer, which
    /// is what makes a reload pick up a wallpaper the user has just set.
    pub(crate) fn background(&self) -> Background {
        self.config
            .background
            .clone()
            .or_else(installed::background)
            .unwrap_or_default()
    }

    /// Whether the background came from a file rather than from the machine.
    ///
    /// The daemon watches `set/` only when this is false; there is no reason
    /// to wake for a directory whose answer is being overridden anyway.
    pub(crate) fn names_background(&self) -> bool {
        self.config.background.is_some()
    }
}

/// The two files this looks at, most specific first.
///
/// Both are returned whether or not they exist, because the watcher needs to
/// know about a file that is *about to* exist -- the commonest way a user
/// configures this is by creating `~/.config/raven/canvas.toml` for the first
/// time, and a watcher that only knew about files present at startup would
/// sleep through it.
pub(crate) fn search_paths(config_home: Option<&Path>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(2);
    if let Some(user) = user_path(config_home, home) {
        paths.push(user);
    }
    paths.push(PathBuf::from(SYSTEM_PATH));
    paths
}

/// The user's config file, from `$XDG_CONFIG_HOME` or `$HOME/.config`.
pub(crate) fn user_path(config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(base) = config_home.filter(|p| !p.as_os_str().is_empty()) {
        return Some(base.join(USER_RELATIVE));
    }
    let home = home.filter(|p| !p.as_os_str().is_empty())?;
    Some(home.join(".config").join(USER_RELATIVE))
}

/// The same, read out of the environment.
pub(crate) fn default_search_paths() -> Vec<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    search_paths(config_home.as_deref(), home.as_deref())
}

/// Read the first of `paths` that exists.
///
/// A file that exists but does not parse is reported through `on_error` and
/// then **skipped**, so a half-edited user file falls through to the system
/// one rather than to nothing. Neither existing is not an error.
pub(crate) fn load_from<F>(paths: &[PathBuf], mut on_error: F) -> Loaded
where
    F: FnMut(&Path, anyhow::Error),
{
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(text) => match parse(&text) {
                Ok(config) => {
                    return Loaded {
                        config,
                        path: Some(path.clone()),
                    };
                }
                Err(e) => on_error(path, e),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => on_error(path, anyhow::Error::new(e).context("cannot read it")),
        }
    }

    Loaded {
        config: Config::default(),
        path: None,
    }
}

/// Parse the text of a config file.
pub(crate) fn parse(text: &str) -> Result<Config> {
    toml::from_str(text).context("this is not a valid canvas.toml")
}

/// Write `config` to `path`, creating the directory if it is missing.
///
/// The file is written whole, with a header saying where it came from. This is
/// what `ravencanvas set --persist` does, and it is why the header exists: a
/// user who finds their hand-written comments gone should at least find out
/// what removed them.
pub(crate) fn save(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let body = toml::to_string_pretty(config).context("cannot serialize the configuration")?;
    let text = format!(
        "# {}\n#\n\
         # Written by `ravencanvas set --persist`. Editing it by hand is fine and\n\
         # supported -- ravencanvasd watches this file and picks up a change within\n\
         # a moment -- but note that the next --persist rewrites the whole file and\n\
         # will not keep comments you add.\n\
         #\n\
         # A file that does not parse is logged and ignored, never fatal: the\n\
         # wallpaper you already have stays on screen while you fix it.\n\n{body}",
        path.display()
    );

    // Written to a temporary file and renamed, so a reader -- which is to say
    // this same daemon, woken by its own write -- never sees half a file.
    let temporary = path.with_extension("toml.new");
    std::fs::write(&temporary, text)
        .with_context(|| format!("cannot write {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("cannot install {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.render.fps, 30);
        assert_eq!(
            config.background, None,
            "an empty file has not chosen a background, which is not the same \
             thing as having chosen the default one"
        );
    }

    /// The built-in is what a file with no `[background]` resolves to on a
    /// machine that has set no wallpaper -- which is every machine this test
    /// suite runs on, since `/usr/share/wallpaper/set` does not exist in a
    /// checkout or in CI.
    #[test]
    fn a_file_that_names_no_background_falls_through_to_the_builtin() {
        let loaded = Loaded {
            config: parse("").unwrap(),
            path: None,
        };
        assert!(!loaded.names_background());
        if installed::wallpaper().is_none() {
            assert!(matches!(loaded.background(), Background::Scene { .. }));
        }
    }

    /// What the file says is used verbatim and the machine's wallpaper is not
    /// consulted at all. This is the half of the precedence rule that must
    /// hold whether or not `set/` exists, so it is asserted unconditionally.
    #[test]
    fn a_file_that_names_a_background_is_not_overridden_by_the_machine() {
        let loaded = Loaded {
            config: parse("[background]\nmode = \"color\"\ncolor = \"#7AA2F7\"\n").unwrap(),
            path: Some(PathBuf::from("/c/raven/canvas.toml")),
        };
        assert!(loaded.names_background());
        assert_eq!(
            loaded.background(),
            Background::Color {
                color: "#7AA2F7".into()
            }
        );
    }

    #[test]
    fn a_scene_parses() {
        // `r##"..."##`: the palette contains `"#`, which ends a plain `r#""#`.
        let config = parse(
            r##"
            [background]
            mode = "scene"
            name = "aurora"
            speed = 0.5
            palette = ["#0A0A12", "#7AA2F7"]
            "##,
        )
        .unwrap();
        assert_eq!(
            config.background,
            Some(Background::Scene {
                name: "aurora".into(),
                speed: 0.5,
                palette: vec!["#0A0A12".into(), "#7AA2F7".into()],
            })
        );
    }

    #[test]
    fn an_image_needs_only_a_path() {
        let config = parse("[background]\nmode = \"image\"\npath = \"/tmp/a.png\"\n").unwrap();
        assert_eq!(
            config.background,
            Some(Background::Image {
                path: "/tmp/a.png".into(),
                fit: "cover".into(),
                background: "#16161F".into(),
            })
        );
    }

    #[test]
    fn a_slideshow_parses_with_its_own_values() {
        let config = parse(
            r#"
            [background]
            mode = "slideshow"
            directory = "/usr/share/backgrounds/raven"
            interval = 300
            shuffle = true
            crossfade = 0

            [render]
            fps = 24
            detail = 720
            "#,
        )
        .unwrap();
        assert_eq!(
            config.render,
            Render {
                fps: 24,
                detail: 720
            }
        );
        let Some(Background::Slideshow {
            interval,
            shuffle,
            crossfade,
            ..
        }) = config.background
        else {
            panic!("not a slideshow");
        };
        assert_eq!((interval, shuffle, crossfade), (300, true, 0));
    }

    /// A typo in a key must be an error rather than silently ignored. The
    /// whole point of a config file is that what you wrote is what happens.
    #[test]
    fn an_unknown_key_is_refused() {
        assert!(parse("[render]\nfsp = 30\n").is_err());
        assert!(parse("[nonsense]\nx = 1\n").is_err());
    }

    #[test]
    fn an_unknown_mode_is_refused() {
        assert!(parse("[background]\nmode = \"video\"\npath = \"/a.mp4\"\n").is_err());
    }

    #[test]
    fn a_config_survives_being_written_and_read_back() {
        let config = Config {
            background: Some(Background::Slideshow {
                directory: "/pics".into(),
                interval: 60,
                shuffle: true,
                crossfade: 400,
                fit: "contain".into(),
                background: "#000000".into(),
            }),
            render: Render {
                fps: 15,
                detail: 240,
            },
        };
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(parse(&text).unwrap(), config);
    }

    // -- the frame interval -------------------------------------------------

    #[test]
    fn the_frame_interval_follows_the_rate() {
        assert_eq!(
            Render { fps: 30, detail: 0 }.frame_interval(),
            std::time::Duration::from_nanos(33_333_333)
        );
    }

    /// Zero would be a division by zero and a million would be a busy loop.
    /// Both are things somebody will eventually type into a config file.
    #[test]
    fn an_absurd_frame_rate_is_clamped_rather_than_obeyed() {
        let slow = Render { fps: 0, detail: 0 }.frame_interval();
        assert_eq!(slow, std::time::Duration::from_secs(1));

        let fast = Render {
            fps: 100_000,
            detail: 0,
        }
        .frame_interval();
        assert!(
            fast >= std::time::Duration::from_nanos(4_000_000),
            "{fast:?}"
        );
    }

    // -- where the files are ------------------------------------------------

    #[test]
    fn the_user_file_comes_from_the_config_home() {
        assert_eq!(
            user_path(Some(Path::new("/home/a/.config")), None),
            Some(PathBuf::from("/home/a/.config/raven/canvas.toml"))
        );
    }

    #[test]
    fn without_a_config_home_it_falls_back_to_dot_config() {
        assert_eq!(
            user_path(None, Some(Path::new("/home/a"))),
            Some(PathBuf::from("/home/a/.config/raven/canvas.toml"))
        );
    }

    #[test]
    fn with_neither_there_is_only_the_system_file() {
        assert_eq!(search_paths(None, None), vec![PathBuf::from(SYSTEM_PATH)]);
    }

    #[test]
    fn the_user_file_is_searched_before_the_system_one() {
        let paths = search_paths(Some(Path::new("/c")), Some(Path::new("/h")));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/c/raven/canvas.toml"),
                PathBuf::from(SYSTEM_PATH)
            ]
        );
    }

    #[test]
    fn an_empty_variable_is_not_a_directory() {
        assert_eq!(
            user_path(Some(Path::new("")), Some(Path::new("/h"))),
            Some("/h/.config/raven/canvas.toml".into())
        );
        assert_eq!(user_path(None, Some(Path::new(""))), None);
    }

    // -- loading, against the real filesystem -------------------------------

    /// A scratch directory that removes itself. Small enough not to be worth a
    /// dependency, and this is the only place that needs one.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ravencanvas-test-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).expect("write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn loading_with_no_files_at_all_gives_the_defaults() {
        let loaded = load_from(&[PathBuf::from("/nonexistent/canvas.toml")], |_, e| {
            panic!("a missing file is not an error: {e}")
        });
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.path, None);
    }

    #[test]
    fn the_first_file_that_exists_wins_outright() {
        let scratch = Scratch::new("precedence");
        let user = scratch.file("user.toml", "[render]\nfps = 12\n");
        let system = scratch.file("system.toml", "[render]\nfps = 99\n");

        let loaded = load_from(&[user.clone(), system], |_, e| panic!("{e}"));
        assert_eq!(loaded.config.render.fps, 12);
        assert_eq!(loaded.path, Some(user));
    }

    /// The case this is written for: somebody is editing their config and has
    /// left it half-typed. The system file must still be found, and the daemon
    /// must be told what was wrong.
    #[test]
    fn a_broken_file_is_reported_and_skipped_rather_than_fatal() {
        let scratch = Scratch::new("broken");
        let user = scratch.file("user.toml", "[background]\nmode = \"scen");
        let system = scratch.file("system.toml", "[render]\nfps = 7\n");

        let mut complaints = Vec::new();
        let loaded = load_from(&[user.clone(), system.clone()], |path, e| {
            complaints.push((path.to_path_buf(), format!("{e:#}")));
        });

        assert_eq!(loaded.config.render.fps, 7);
        assert_eq!(loaded.path, Some(system));
        assert_eq!(complaints.len(), 1);
        assert_eq!(complaints[0].0, user);
        assert!(
            complaints[0].1.contains("canvas.toml"),
            "{}",
            complaints[0].1
        );
    }

    #[test]
    fn every_file_being_broken_falls_back_to_the_defaults() {
        let scratch = Scratch::new("all-broken");
        let only = scratch.file("only.toml", "!!!not toml!!!");
        let mut complaints = 0;
        let loaded = load_from(&[only], |_, _| complaints += 1);

        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.path, None, "nothing was successfully read");
        assert_eq!(complaints, 1);
    }

    #[test]
    fn saving_creates_the_directory_and_writes_something_readable_back() {
        let scratch = Scratch::new("save");
        let path = scratch.0.join("nested/deeper/canvas.toml");

        let config = Config {
            background: Some(Background::Color {
                color: "#7AA2F7".into(),
            }),
            render: Render { fps: 20, detail: 0 },
        };
        save(&path, &config).expect("save");

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.starts_with("# "), "the header is missing: {text}");
        assert!(text.contains("ravencanvas set --persist"), "{text}");
        assert_eq!(parse(&text).unwrap(), config);
    }

    /// The trap `background`'s `skip_serializing_if` exists for: TOML cannot
    /// write a null, so a `Config` that has not chosen a background must come
    /// out as a file with no `[background]` table rather than as an error.
    #[test]
    fn a_config_with_no_background_can_still_be_written_and_read_back() {
        let scratch = Scratch::new("no-background");
        let path = scratch.0.join("canvas.toml");

        let config = Config {
            background: None,
            render: Render { fps: 24, detail: 0 },
        };
        save(&path, &config).expect("save");

        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !text.contains("[background]"),
            "a background nobody chose was written down: {text}"
        );
        assert_eq!(parse(&text).unwrap(), config);
    }

    /// Written through a rename, so the daemon woken by its own write never
    /// reads a half-written file. The temporary must not be left behind.
    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let scratch = Scratch::new("atomic");
        let path = scratch.0.join("canvas.toml");
        save(&path, &Config::default()).expect("save");

        assert!(path.exists());
        assert!(
            !path.with_extension("toml.new").exists(),
            "a temporary was left behind"
        );
    }
}
