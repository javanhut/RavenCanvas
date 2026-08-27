//! Noticing that a file on disk changed.
//!
//! Two things are watched: the config files, and the slideshow directory.
//!
//! # Directories, not files
//!
//! Every watch here is on a *directory*, and the events are filtered by name
//! afterwards. Watching the file itself looks more direct and is wrong for two
//! reasons, both of which happen constantly:
//!
//! - **The file may not exist yet.** The commonest way somebody configures
//!   this daemon is by creating `~/.config/raven/canvas.toml` for the first
//!   time. There is nothing to put a watch on until they do.
//! - **Editors do not modify files, they replace them.** `vim`, `helix` and
//!   anything else that writes safely does the same thing this daemon's own
//!   `config::save` does: write a temporary and rename it over the original. A
//!   watch on the original follows the *old inode*, which has just been
//!   unlinked, and never fires again. The file is edited ten more times and
//!   the daemon sleeps through all of it.
//!
//! Watching the directory catches the create, the rename and the delete alike,
//! and survives all three.
//!
//! # Debouncing is the caller's job
//!
//! A single save produces several events. This reports them all; `app`
//! collapses them by re-reading the config and doing nothing when it has not
//! changed, which is a better filter than a timer -- it is exact, and it costs
//! a file read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use inotify::{Inotify, WatchDescriptor, WatchMask};

/// What is worth waking up for.
///
/// `CLOSE_WRITE` rather than `MODIFY`: a program writing a file in pieces
/// produces a `MODIFY` per write, and reacting to the first one reads a
/// half-written file. `MOVED_TO` is the rename half of a safe write, and
/// `CREATE` and `DELETE` are how a slideshow directory gains and loses
/// pictures.
const EVENTS: WatchMask = WatchMask::CLOSE_WRITE
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::CREATE)
    .union(WatchMask::DELETE)
    .union(WatchMask::DELETE_SELF)
    .union(WatchMask::MOVE_SELF);

/// The buffer one batch of events is read into.
///
/// An inotify event is 16 bytes plus a name. 4 KiB holds a few dozen, and the
/// read loop drains until it would block, so this is a batch size rather than
/// a limit.
const BUFFER: usize = 4096;

/// A set of watched directories.
pub(crate) struct Watcher {
    inotify: Inotify,
    /// Which directory each descriptor is for, so an event can be reported by
    /// path rather than by an opaque handle.
    watched: HashMap<WatchDescriptor, PathBuf>,
    buffer: Vec<u8>,
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher")
            .field("directories", &self.watched.values().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Watcher {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            inotify: Inotify::init().context("cannot create an inotify instance")?,
            watched: HashMap::new(),
            buffer: vec![0; BUFFER],
        })
    }

    /// A duplicate of the inotify descriptor, for the event loop to poll.
    ///
    /// See [`crate::control::Listener::try_clone_fd`] for why it is a
    /// duplicate.
    pub(crate) fn try_clone_fd(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        use std::os::fd::AsFd;
        self.inotify.as_fd().try_clone_to_owned()
    }

    /// Watch exactly `directories` and nothing else.
    ///
    /// Called again whenever the set changes -- switching to a slideshow adds
    /// its directory, switching away drops it. Directories that cannot be
    /// watched are reported once and skipped: a slideshow pointed at a
    /// removable drive that is not plugged in is a normal state, not a
    /// failure.
    pub(crate) fn watch(&mut self, directories: &[PathBuf]) {
        let mut watches = self.inotify.watches();
        for (descriptor, path) in self.watched.drain() {
            if let Err(e) = watches.remove(descriptor) {
                tracing::debug!(path = %path.display(), "cannot drop a watch: {e}");
            }
        }

        let mut watched = HashMap::new();
        for directory in directories {
            match watches.add(directory, EVENTS) {
                Ok(descriptor) => {
                    tracing::debug!(path = %directory.display(), "watching");
                    watched.insert(descriptor, directory.clone());
                }
                Err(e) => tracing::debug!(
                    path = %directory.display(),
                    "not watching this directory: {e}"
                ),
            }
        }
        self.watched = watched;
    }

    /// Every path that changed since the last call.
    ///
    /// Paths, not events: what the caller needs to know is *which file*, and
    /// which of `create`, `moved_to` and `close_write` produced it is an
    /// implementation detail of whichever editor was used.
    ///
    /// An event with no name -- the directory itself was deleted or moved --
    /// is reported as the directory's own path, so a caller watching a
    /// slideshow directory finds out that it has gone.
    pub(crate) fn drain(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        loop {
            let events = match self.inotify.read_events(&mut self.buffer) {
                Ok(events) => events,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!("cannot read inotify events: {e}");
                    break;
                }
            };

            let mut any = false;
            for event in events {
                any = true;
                let Some(directory) = self.watched.get(&event.wd) else {
                    continue;
                };
                changed.push(match event.name {
                    Some(name) => directory.join(name),
                    None => directory.clone(),
                });
            }
            if !any {
                break;
            }
        }

        changed.sort();
        changed.dedup();
        changed
    }
}

/// The directories that have to be watched to notice a change to any of
/// `files`.
///
/// Deduplicated, because the user's config and the system's are usually in
/// different directories but a slideshow of `~/.config/raven` would not be.
pub(crate) fn directories_of(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = files
        .iter()
        .filter_map(|file| file.parent())
        .map(Path::to_path_buf)
        .collect();
    directories.sort();
    directories.dedup();
    directories
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A scratch directory, removed with the test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ravencanvas-watch-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Poll until something is reported, or give up.
    ///
    /// inotify delivers to a descriptor rather than synchronously, so a test
    /// that reads once immediately after writing is a flake waiting to happen.
    fn wait_for_change(watcher: &mut Watcher) -> Vec<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let changed = watcher.drain();
            if !changed.is_empty() || Instant::now() > deadline {
                return changed;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn the_directories_of_some_files_are_deduplicated() {
        let files = vec![
            PathBuf::from("/a/canvas.toml"),
            PathBuf::from("/a/other.toml"),
            PathBuf::from("/etc/raven/canvas.toml"),
        ];
        assert_eq!(
            directories_of(&files),
            vec![PathBuf::from("/a"), PathBuf::from("/etc/raven")]
        );
    }

    #[test]
    fn a_file_with_no_parent_is_skipped_rather_than_panicking() {
        assert!(directories_of(&[PathBuf::from("/")]).is_empty());
    }

    #[test]
    fn a_new_file_is_noticed() {
        let scratch = Scratch::new("create");
        let mut watcher = Watcher::new().expect("inotify");
        watcher.watch(std::slice::from_ref(&scratch.0));

        std::fs::write(scratch.0.join("canvas.toml"), "[render]\nfps = 10\n").expect("write");

        let changed = wait_for_change(&mut watcher);
        assert!(
            changed.contains(&scratch.0.join("canvas.toml")),
            "got {changed:?}"
        );
    }

    /// The case that motivates watching directories: an editor writes a
    /// temporary file and renames it over the original. A watch on the
    /// original would follow the unlinked inode and never fire again.
    #[test]
    fn a_file_replaced_by_a_rename_is_noticed() {
        let scratch = Scratch::new("rename");
        let target = scratch.0.join("canvas.toml");
        std::fs::write(&target, "old").expect("write");

        let mut watcher = Watcher::new().expect("inotify");
        watcher.watch(std::slice::from_ref(&scratch.0));

        let temporary = scratch.0.join("canvas.toml.new");
        std::fs::write(&temporary, "new").expect("write");
        std::fs::rename(&temporary, &target).expect("rename");

        let changed = wait_for_change(&mut watcher);
        assert!(
            changed.contains(&target),
            "the rename was missed: {changed:?}"
        );
    }

    #[test]
    fn a_deleted_file_is_noticed() {
        let scratch = Scratch::new("delete");
        let target = scratch.0.join("gone.png");
        std::fs::write(&target, "x").expect("write");

        let mut watcher = Watcher::new().expect("inotify");
        watcher.watch(std::slice::from_ref(&scratch.0));
        std::fs::remove_file(&target).expect("remove");

        assert!(wait_for_change(&mut watcher).contains(&target));
    }

    #[test]
    fn draining_with_nothing_to_report_returns_nothing_and_does_not_block() {
        let scratch = Scratch::new("quiet");
        let mut watcher = Watcher::new().expect("inotify");
        watcher.watch(std::slice::from_ref(&scratch.0));
        assert!(watcher.drain().is_empty());
    }

    /// A slideshow pointed at a drive that is not plugged in is a normal
    /// state. It must not stop the other watches from being registered.
    #[test]
    fn a_missing_directory_is_skipped_rather_than_failing_the_whole_set() {
        let scratch = Scratch::new("missing");
        let mut watcher = Watcher::new().expect("inotify");
        watcher.watch(&[PathBuf::from("/nonexistent/wallpapers"), scratch.0.clone()]);

        std::fs::write(scratch.0.join("a.png"), "x").expect("write");
        assert!(
            !wait_for_change(&mut watcher).is_empty(),
            "the good watch was lost"
        );
    }

    #[test]
    fn rewatching_replaces_the_previous_set() {
        let first = Scratch::new("rewatch-a");
        let second = Scratch::new("rewatch-b");

        let mut watcher = Watcher::new().expect("inotify");
        watcher.watch(std::slice::from_ref(&first.0));
        watcher.watch(std::slice::from_ref(&second.0));

        // A change in the directory that is no longer watched must be silent.
        std::fs::write(first.0.join("ignored.png"), "x").expect("write");
        std::thread::sleep(Duration::from_millis(50));
        assert!(watcher.drain().is_empty(), "the old watch is still live");

        std::fs::write(second.0.join("noticed.png"), "x").expect("write");
        assert!(wait_for_change(&mut watcher).contains(&second.0.join("noticed.png")));
    }
}
