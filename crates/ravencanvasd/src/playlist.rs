//! The images in a slideshow directory, and the order they are shown in.
//!
//! # Why the order is computed rather than random
//!
//! A shuffle here is a *permutation chosen once per pass*, not a random pick
//! each time the timer fires. Picking independently is what produces the
//! complaint every slideshow eventually gets -- "it keeps showing me the same
//! three pictures" -- because independent picks from twenty images repeat a
//! recent one about a quarter of the time. A permutation shows every image
//! exactly once before any of them comes round again.
//!
//! The permutation is a seeded Fisher-Yates, and the seed changes each pass,
//! so two passes are not the same order and neither is two sessions. Seeded
//! rather than drawn from a random source because it makes the whole thing a
//! function of its inputs, which is what lets the tests below assert on orders
//! rather than on distributions.

use std::path::{Path, PathBuf};

/// The extensions a slideshow will consider.
///
/// Matched case-insensitively. This is a filter on *what to try*, not a
/// promise about what will decode -- the decoder still looks at the file's
/// magic number, and a `.png` that is really a JPEG works. It exists because a
/// directory listing is cheap and decoding every file in somebody's Pictures
/// folder to find out which are images is not.
const EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "jpe", "jfif"];

/// The files in a slideshow, and where in them we are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Playlist {
    /// Every image found, sorted by path. Sorted rather than in directory
    /// order because directory order is the filesystem's business and differs
    /// between machines, which would make an unshuffled slideshow arbitrary.
    entries: Vec<PathBuf>,
    /// Indices into `entries`, in the order they will be shown.
    order: Vec<usize>,
    /// Where in `order` we are.
    position: usize,
    shuffle: bool,
    /// Advanced every time the order is regenerated, so successive passes
    /// differ.
    pass: u64,
}

impl Playlist {
    /// Build a playlist from a list of files.
    ///
    /// `seed` is mixed into the shuffle. Callers pass something that varies
    /// between runs; the tests pass a constant.
    pub(crate) fn new(mut entries: Vec<PathBuf>, shuffle: bool, seed: u64) -> Self {
        entries.sort();
        entries.dedup();

        let mut playlist = Self {
            entries,
            order: Vec::new(),
            position: 0,
            shuffle,
            pass: seed,
        };
        playlist.reorder();
        playlist
    }

    /// Scan `directory` for images, one level deep.
    ///
    /// One level: a wallpaper directory is a wallpaper directory, and
    /// recursing turns "point this at Pictures" into a walk of somebody's
    /// entire photo library. Unreadable entries are skipped rather than
    /// failing the scan -- half a directory of wallpapers is still a
    /// slideshow.
    pub(crate) fn scan(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(directory)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();

            // `file_type` rather than `metadata`, so a symlink to a file the
            // daemon cannot stat does not abort the scan. A symlink into a
            // wallpaper collection is a normal way to build one of these.
            let is_file = entry
                .file_type()
                .map(|kind| kind.is_file() || kind.is_symlink())
                .unwrap_or(false);
            if is_file && has_image_extension(&path) {
                found.push(path);
            }
        }
        Ok(found)
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The image that should be on screen.
    #[must_use]
    pub(crate) fn current(&self) -> Option<&Path> {
        let index = *self.order.get(self.position)?;
        self.entries.get(index).map(PathBuf::as_path)
    }

    /// Move `by` places, wrapping, and return the new current image.
    ///
    /// Wrapping past either end regenerates the order when shuffling, so each
    /// pass through the directory is a different permutation. It does *not*
    /// regenerate when the order is alphabetical, because "wrap round to the
    /// first one" is the whole of what alphabetical order means.
    pub(crate) fn advance(&mut self, by: i32) -> Option<&Path> {
        if self.order.is_empty() {
            return None;
        }

        let length = self.order.len() as i64;
        let target = self.position as i64 + i64::from(by);
        if self.shuffle && (target < 0 || target >= length) {
            self.pass = self.pass.wrapping_add(1);
            self.reorder();
        }
        self.position = target.rem_euclid(length) as usize;
        self.current()
    }

    /// Replace the file list, keeping the current image on screen if it is
    /// still there.
    ///
    /// This is what a rescan calls. Keeping position matters: a slideshow that
    /// jumped back to the first image every time somebody saved a file into
    /// the directory would be maddening, and a directory being watched is
    /// exactly a directory that changes.
    pub(crate) fn replace(&mut self, entries: Vec<PathBuf>) {
        let showing = self.current().map(Path::to_path_buf);
        let shuffle = self.shuffle;
        let pass = self.pass;

        *self = Self::new(entries, shuffle, pass);

        if let Some(showing) = showing
            && let Some(index) = self.entries.iter().position(|entry| *entry == showing)
            && let Some(place) = self.order.iter().position(|&slot| slot == index)
        {
            self.position = place;
        }
    }

    /// Regenerate `order` for the current `pass`.
    fn reorder(&mut self) {
        self.order = (0..self.entries.len()).collect();
        if !self.shuffle || self.order.len() < 2 {
            return;
        }

        // Fisher-Yates, back to front, with the swap partner drawn from a
        // hash of the pass and the position. Every permutation is reachable
        // and the result is a pure function of `(len, pass)`.
        let mut state = splitmix(self.pass ^ 0x5761_6C6C_7061_7065);
        for i in (1..self.order.len()).rev() {
            state = splitmix(state);
            let j = (state % (i as u64 + 1)) as usize;
            self.order.swap(i, j);
        }
    }
}

/// Whether a path looks like an image this can decode.
fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

/// `splitmix64`. A one-line generator with good enough statistics for
/// deciding what order to show somebody's holiday photographs in, and no
/// dependency.
fn splitmix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    fn ordered(playlist: &mut Playlist, steps: usize) -> Vec<PathBuf> {
        let mut seen = Vec::new();
        for _ in 0..steps {
            seen.push(playlist.current().expect("a current image").to_path_buf());
            playlist.advance(1);
        }
        seen
    }

    // -- extensions ---------------------------------------------------------

    #[test]
    fn image_extensions_are_recognised_whatever_their_case() {
        for name in ["a.png", "a.PNG", "a.jpg", "a.JPEG", "a.jpe", "a.jfif"] {
            assert!(has_image_extension(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn other_files_are_not_images() {
        for name in ["notes.txt", "a.mp4", "a.png.bak", "README", ".hidden", "a."] {
            assert!(!has_image_extension(Path::new(name)), "{name}");
        }
    }

    // -- ordering -----------------------------------------------------------

    #[test]
    fn an_unshuffled_playlist_is_alphabetical() {
        let mut playlist = Playlist::new(files(&["/c.png", "/a.png", "/b.png"]), false, 0);
        assert_eq!(
            ordered(&mut playlist, 3),
            files(&["/a.png", "/b.png", "/c.png"])
        );
    }

    #[test]
    fn an_unshuffled_playlist_wraps_to_the_start() {
        let mut playlist = Playlist::new(files(&["/a.png", "/b.png"]), false, 0);
        assert_eq!(
            ordered(&mut playlist, 5),
            files(&["/a.png", "/b.png", "/a.png", "/b.png", "/a.png"])
        );
    }

    #[test]
    fn advancing_backwards_wraps_the_other_way() {
        let mut playlist = Playlist::new(files(&["/a.png", "/b.png", "/c.png"]), false, 0);
        assert_eq!(playlist.advance(-1), Some(Path::new("/c.png")));
        assert_eq!(playlist.advance(-1), Some(Path::new("/b.png")));
    }

    #[test]
    fn advancing_by_more_than_the_length_still_lands_somewhere_real() {
        let mut playlist = Playlist::new(files(&["/a.png", "/b.png", "/c.png"]), false, 0);
        assert_eq!(
            playlist.advance(7),
            Some(Path::new("/b.png")),
            "0 + 7 wraps to 1"
        );
        assert_eq!(
            playlist.advance(-7),
            Some(Path::new("/a.png")),
            "1 - 7 wraps to 0"
        );
    }

    /// The property the whole shuffle exists for: every image is shown once
    /// before any is shown twice.
    #[test]
    fn a_shuffled_pass_shows_every_image_exactly_once() {
        let names = files(&["/a.png", "/b.png", "/c.png", "/d.png", "/e.png", "/f.png"]);
        let mut playlist = Playlist::new(names.clone(), true, 1);

        let mut pass = ordered(&mut playlist, 6);
        pass.sort();
        assert_eq!(pass, names);
    }

    #[test]
    fn a_shuffle_actually_reorders() {
        let names = files(&[
            "/a.png", "/b.png", "/c.png", "/d.png", "/e.png", "/f.png", "/g.png",
        ]);
        // At least one seed out of a handful must produce a different order;
        // if none does, the shuffle is not shuffling.
        let reordered = (0..8u64).any(|seed| {
            let mut playlist = Playlist::new(names.clone(), true, seed);
            ordered(&mut playlist, names.len()) != names
        });
        assert!(reordered, "no seed changed the order");
    }

    #[test]
    fn successive_passes_are_different_permutations() {
        let names = files(&[
            "/a.png", "/b.png", "/c.png", "/d.png", "/e.png", "/f.png", "/g.png", "/h.png",
        ]);
        let mut playlist = Playlist::new(names.clone(), true, 4);
        let first = ordered(&mut playlist, names.len());
        let second = ordered(&mut playlist, names.len());
        assert_ne!(first, second, "two passes ran in the same order");
    }

    #[test]
    fn a_shuffled_playlist_is_a_function_of_its_seed() {
        let names = files(&["/a.png", "/b.png", "/c.png", "/d.png", "/e.png"]);
        let mut one = Playlist::new(names.clone(), true, 99);
        let mut two = Playlist::new(names, true, 99);
        assert_eq!(ordered(&mut one, 5), ordered(&mut two, 5));
    }

    #[test]
    fn an_unshuffled_playlist_does_not_reorder_when_it_wraps() {
        let names = files(&["/a.png", "/b.png", "/c.png"]);
        let mut playlist = Playlist::new(names, false, 0);
        let first = ordered(&mut playlist, 3);
        let second = ordered(&mut playlist, 3);
        assert_eq!(first, second);
    }

    // -- edges --------------------------------------------------------------

    #[test]
    fn an_empty_playlist_has_nothing_to_show_and_does_not_panic() {
        let mut playlist = Playlist::new(Vec::new(), true, 0);
        assert!(playlist.is_empty());
        assert_eq!(playlist.len(), 0);
        assert_eq!(playlist.current(), None);
        assert_eq!(playlist.advance(1), None);
        assert_eq!(playlist.advance(-5), None);
    }

    #[test]
    fn a_single_image_playlist_stays_on_it() {
        let mut playlist = Playlist::new(files(&["/only.png"]), true, 3);
        for _ in 0..5 {
            assert_eq!(playlist.advance(1), Some(Path::new("/only.png")));
        }
    }

    #[test]
    fn duplicates_are_collapsed() {
        let playlist = Playlist::new(files(&["/a.png", "/a.png", "/b.png"]), false, 0);
        assert_eq!(playlist.len(), 2);
    }

    // -- rescanning ---------------------------------------------------------

    /// A directory being watched is a directory that changes. Jumping back to
    /// the first image every time somebody saves a file into it would be the
    /// most irritating possible behaviour.
    #[test]
    fn a_rescan_keeps_showing_the_same_image() {
        let mut playlist = Playlist::new(files(&["/a.png", "/b.png", "/c.png"]), false, 0);
        playlist.advance(1);
        assert_eq!(playlist.current(), Some(Path::new("/b.png")));

        playlist.replace(files(&["/a.png", "/b.png", "/c.png", "/d.png"]));
        assert_eq!(playlist.current(), Some(Path::new("/b.png")));
        assert_eq!(playlist.len(), 4);
    }

    #[test]
    fn a_rescan_that_removes_the_current_image_lands_somewhere_valid() {
        let mut playlist = Playlist::new(files(&["/a.png", "/b.png", "/c.png"]), false, 0);
        playlist.advance(1);
        playlist.replace(files(&["/a.png", "/c.png"]));

        let current = playlist.current().expect("something must be showing");
        assert!(
            playlist.entries.iter().any(|entry| entry == current),
            "{current:?}"
        );
    }

    #[test]
    fn a_rescan_to_nothing_leaves_an_empty_playlist_rather_than_panicking() {
        let mut playlist = Playlist::new(files(&["/a.png"]), false, 0);
        playlist.replace(Vec::new());
        assert!(playlist.is_empty());
        assert_eq!(playlist.current(), None);
    }

    // -- scanning a real directory ------------------------------------------

    #[test]
    fn scanning_finds_images_and_ignores_everything_else() {
        let directory = std::env::temp_dir().join("ravencanvas-test-scan");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(directory.join("subdir")).expect("scratch");

        for name in ["one.png", "two.JPG", "notes.txt", "movie.mp4"] {
            std::fs::write(directory.join(name), b"x").expect("write");
        }
        std::fs::write(directory.join("subdir/deep.png"), b"x").expect("write");

        let mut found = Playlist::scan(&directory).expect("scan");
        found.sort();
        let names: Vec<String> = found
            .iter()
            .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
            .collect();

        assert_eq!(names, vec!["one.png".to_string(), "two.JPG".to_string()]);

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn scanning_a_missing_directory_is_an_error_rather_than_an_empty_list() {
        // The distinction matters: an empty directory is a slideshow with
        // nothing in it, and a missing one is a mistake worth a log line.
        assert!(Playlist::scan(Path::new("/nonexistent/wallpapers")).is_err());
    }
}
