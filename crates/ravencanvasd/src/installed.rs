//! The wallpaper this machine has set, as opposed to the one this user asked
//! for.
//!
//! # The contract this implements
//!
//! `/usr/share/wallpaper` is the library of images an installation ships or
//! collects. `set/` holds exactly one of them, under the name `wallpaper` with
//! whatever extension the image arrived with -- a copy or a symlink, either
//! counts, because this follows them.
//!
//! That path is not this daemon's invention and is not a preference belonging
//! to it. RavenLogin's `login.toml` already describes it as *"the wallpaper
//! this machine has set, and the same file huginn draws behind the session"*,
//! and its greeter reads it whenever `login.toml` names no wallpaper of its
//! own. Until now nothing on the session side of that sentence was true: a
//! machine that set a wallpaper got it on the login screen and the built-in
//! gradient on the desktop, which is the one outcome the contract exists to
//! prevent. This module is the other half.
//!
//! # Why it is a fallback and not a default
//!
//! It is consulted only when no config file named a background -- see
//! [`crate::config::Loaded::background`]. The order is the same one the
//! greeter uses and it reads the same way in both places: what the person
//! wrote wins over what the machine has, and the built-in only appears when
//! there is neither.
//!
//! # Why every failure here is silent
//!
//! A missing directory, an unreadable one, a `set/` nobody has put anything
//! in: all of them return `None` without a word. That is the opposite of how
//! [`crate::config`] treats a path somebody wrote down and got wrong, and the
//! difference is the point. A configured path that does not resolve is a
//! mistake worth complaining about. An empty `set/` is the ordinary state of a
//! machine that has never chosen a wallpaper, and a daemon that logged a
//! warning about it would log one on every start on most machines.

use std::path::{Path, PathBuf};

use raven_canvas_proto::Background;

/// The directory holding the wallpaper this machine has set.
///
/// Compiled in rather than made another key in `canvas.toml`, deliberately,
/// and for the reason `login.toml` gives for doing the same: this is a
/// contract between the login screen and the session that follows it, so it
/// belongs to neither of them. Somebody who wants this user's desktop to
/// differ from the machine's wallpaper already has `[background]` to say so,
/// and it wins.
const SET_DIR: &str = "/usr/share/wallpaper/set";

/// The basename of the active wallpaper inside [`SET_DIR`].
const SET_STEM: &str = "wallpaper";

/// How the fallback is fitted.
///
/// "Cover" rather than "contain": a letterboxed wallpaper looks like a
/// mistake, and the bars would be the one part of the screen not matching the
/// login screen this is supposed to agree with -- the greeter crops from the
/// centre too.
///
/// These two are the same strings `Background`'s serde defaults produce for an
/// `[background]` that names only a path, and a test below pins them to it. A
/// wallpaper picked up from `set/` and the same file written into a config
/// file by hand must not be drawn differently.
const FIT: &str = "cover";

/// What is shown wherever the image does not reach.
///
/// Unreachable under [`FIT`], which leaves no gap. It is carried anyway
/// because `Background::Image` requires it, and it is huginn's own background
/// colour so that it matches the desktop if a future fit ever exposes it.
const BACKGROUND: &str = "#16161F";

/// The directory to watch for a change to the machine's wallpaper.
///
/// Watched only while the fallback is in use; see [`crate::app::App::rewatch`].
/// Handed out as a path rather than by watching from in here because the
/// daemon keeps every inotify watch in one place.
pub(crate) fn set_dir() -> PathBuf {
    PathBuf::from(SET_DIR)
}

/// The wallpaper this machine has set, if it has one.
pub(crate) fn wallpaper() -> Option<PathBuf> {
    wallpaper_in(Path::new(SET_DIR))
}

/// The same, against a directory that is passed in.
///
/// Split from [`wallpaper`] the way [`crate::config::search_paths`] is split
/// from `default_search_paths`, and for the same reason: a function that reads
/// a compiled-in path can only be tested on a machine that happens to have one,
/// and a function that is *given* the directory can be tested against a scratch
/// one holding exactly the awkward cases.
fn wallpaper_in(directory: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(directory).ok()?;
    choose(
        entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            // Follows symlinks, so `set/wallpaper.jpg -> ../cliff.jpg` is a
            // file by this test and a directory called `wallpaper.d` is not.
            // A symlink to nothing is not a file either, which is the right
            // answer for a `set/` pointing into a drive that is not mounted.
            .filter(|path| path.is_file()),
    )
}

/// The background to draw for it, if there is one.
pub(crate) fn background() -> Option<Background> {
    let path = wallpaper()?;
    tracing::info!(
        path = %path.display(),
        "no background configured; using the wallpaper this machine has set"
    );
    Some(background_for(path))
}

/// Pick the active wallpaper out of the names in [`SET_DIR`].
///
/// Split from [`wallpaper`] so the rule is testable without a filesystem.
/// Sorted, because `read_dir` yields whatever order the filesystem feels like
/// and a directory holding two of these should not mean a desktop that changes
/// between logins. More than one `wallpaper.*` in `set/` is a mistake however
/// it is resolved; sorting at least makes it the same mistake twice, and the
/// same one the greeter makes, so the login screen and the desktop still agree.
fn choose(entries: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = entries
        .filter(|path| path.file_stem().is_some_and(|stem| stem == SET_STEM))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// The background for one wallpaper path.
///
/// The extension is not consulted and is not what decides the format:
/// `raven_paint::Image::decode` reads that out of the first bytes, which is
/// both because an extension is a claim anybody can get wrong and because
/// dispatching a parser on a filename is how a parser ends up being handed a
/// file the caller did not think it was handing it.
fn background_for(path: impl Into<PathBuf>) -> Background {
    Background::Image {
        path: path.into(),
        fit: FIT.to_string(),
        background: BACKGROUND.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|name| Path::new(SET_DIR).join(name))
            .collect()
    }

    #[test]
    fn the_wallpaper_is_the_one_named_wallpaper() {
        let chosen = choose(paths(&["README", "wallpaper.png", "cliff.jpg"]).into_iter());
        assert_eq!(chosen, Some(Path::new(SET_DIR).join("wallpaper.png")));
    }

    /// The extension is whatever the image arrived with, so nothing may key
    /// off a fixed list of them.
    #[test]
    fn any_extension_counts() {
        for name in [
            "wallpaper.png",
            "wallpaper.jpg",
            "wallpaper.jpeg",
            "wallpaper",
        ] {
            assert_eq!(
                choose(paths(&[name]).into_iter()),
                Some(Path::new(SET_DIR).join(name)),
                "{name} should have been chosen"
            );
        }
    }

    #[test]
    fn a_directory_with_nothing_named_wallpaper_has_none() {
        assert_eq!(
            choose(paths(&["cliff.jpg", "wallpapers.png"]).into_iter()),
            None
        );
        assert_eq!(choose(std::iter::empty()), None);
    }

    /// `wallpaper.d` and `wallpaperish.png` both have a stem that merely starts
    /// with the right word. Neither is the wallpaper.
    #[test]
    fn a_name_that_only_starts_with_wallpaper_is_not_it() {
        assert_eq!(
            choose(paths(&["wallpaperish.png", "wallpaper-old.jpg"]).into_iter()),
            None
        );
    }

    /// Two of these is a mistake, but it must be a stable one: a desktop that
    /// changes picture between logins because `read_dir` returned a different
    /// order is worse than one that is consistently showing the wrong file.
    #[test]
    fn two_candidates_resolve_the_same_way_every_time() {
        let forwards = choose(paths(&["wallpaper.jpg", "wallpaper.png"]).into_iter());
        let backwards = choose(paths(&["wallpaper.png", "wallpaper.jpg"]).into_iter());
        assert_eq!(forwards, backwards);
        assert_eq!(forwards, Some(Path::new(SET_DIR).join("wallpaper.jpg")));
    }

    /// The pin promised by [`FIT`]'s documentation. A wallpaper picked up from
    /// `set/` must be drawn exactly as the same file named in a config file
    /// would be, which means these constants must stay equal to `Background`'s
    /// own serde defaults rather than merely looking like them.
    #[test]
    fn the_fallback_matches_a_hand_written_image_stanza() {
        let written: Background =
            toml::from_str("mode = \"image\"\npath = \"/usr/share/wallpaper/set/wallpaper.png\"\n")
                .expect("a minimal image stanza");
        assert_eq!(
            background_for("/usr/share/wallpaper/set/wallpaper.png"),
            written
        );
    }

    /// The real directory, whatever is in it. The only thing asserted is that
    /// a machine without one -- which is most of them, and every CI runner --
    /// gets silence rather than a panic.
    #[test]
    fn a_missing_set_directory_is_not_an_error() {
        let _ = wallpaper();
        let _ = background();
    }

    // -- against a real directory -------------------------------------------

    /// A scratch directory that removes itself, as in [`crate::config`]'s
    /// tests. Small enough not to be worth sharing between two modules.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ravencanvas-installed-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"not really an image").expect("write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_real_directory_yields_the_file_named_wallpaper() {
        let scratch = Scratch::new("plain");
        scratch.file("notes.txt");
        let wanted = scratch.file("wallpaper.png");

        assert_eq!(wallpaper_in(&scratch.0), Some(wanted));
    }

    #[test]
    fn an_empty_directory_yields_nothing() {
        let scratch = Scratch::new("empty");
        assert_eq!(wallpaper_in(&scratch.0), None);
    }

    /// The documented shape of `set/`: a symlink into the library one
    /// directory up, rather than a copy. `is_file` follows it, so this is the
    /// case that must work.
    #[test]
    fn a_symlink_into_the_library_counts() {
        let scratch = Scratch::new("symlink");
        let real = scratch.file("cliff.jpg");
        let link = scratch.0.join("wallpaper.jpg");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        assert_eq!(wallpaper_in(&scratch.0), Some(link));
    }

    /// A `set/` pointing at something that is not there -- an unplugged drive,
    /// a file somebody deleted out from under the link. Not a wallpaper, and
    /// not a panic: the caller falls through to the built-in.
    #[test]
    fn a_symlink_to_nothing_is_not_a_wallpaper() {
        let scratch = Scratch::new("dangling");
        std::os::unix::fs::symlink(scratch.0.join("gone.png"), scratch.0.join("wallpaper.png"))
            .expect("symlink");

        assert_eq!(wallpaper_in(&scratch.0), None);
    }

    /// `wallpaper.d/` has exactly the right stem and is not a file.
    #[test]
    fn a_directory_named_wallpaper_is_not_a_wallpaper() {
        let scratch = Scratch::new("directory");
        std::fs::create_dir(scratch.0.join("wallpaper.d")).expect("mkdir");

        assert_eq!(wallpaper_in(&scratch.0), None);
    }
}
