//! The daemon's state, and everything Wayland says to it.
//!
//! This is the glue: `engine` decides what should be on screen, `screen`
//! knows how to put it there, and this holds one of the first and several of
//! the second while a compositor, a control socket and an inotify descriptor
//! all try to interrupt.
//!
//! # No seat, no input
//!
//! There is no [`SeatHandler`] here and no `wl_seat` is ever bound. A
//! wallpaper takes no keyboard and no pointer; `screen` sets an empty input
//! region so clicks fall through to the desktop, and not binding a seat at all
//! is the same statement made where it cannot be undone by accident.
//!
//! # Multiple outputs
//!
//! One layer surface per `wl_output`, each with its own size, its own scaled
//! copy of the wallpaper, and its own scene field. RavenGUI's
//! `docs/integration.md` says huginn is single-output today and that the
//! output argument to `get_layer_surface` is currently ignored, so on Raven
//! this loop runs once. It is written this way regardless, because "one
//! screen" is a property of today's compositor rather than of wallpapers, and
//! a `Vec` of one is not more complicated than a field.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use raven_canvas_proto::{Background, OutputStatus, Request, Response, SceneInfo, Status};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::GlobalList;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShell, LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};

use crate::config::{self, Config};
use crate::control::{self, Listener};
use crate::engine::Engine;
use crate::installed;
use crate::screen::{self, Screen};
use crate::watch::{self, Watcher};

/// Everything the daemon is.
pub(crate) struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,

    engine: Engine,
    screens: Vec<Screen>,

    /// Every file that could be the configuration, most specific first.
    config_paths: Vec<PathBuf>,
    /// The one actually read, if any.
    config_path: Option<PathBuf>,
    /// Where `--persist` writes. `None` when there is no home directory to
    /// write into, which makes persisting an error rather than a surprise.
    user_config_path: Option<PathBuf>,
    /// Whether a config file named the background, rather than it coming from
    /// the wallpaper this machine has set.
    ///
    /// Only [`App::rewatch`] and [`App::drain_watches`] care: when this is
    /// false the daemon also watches `/usr/share/wallpaper/set`, so that
    /// changing the machine's wallpaper changes the desktop without a logout.
    /// When it is true that directory is being overridden and is not worth
    /// waking for.
    config_names_background: bool,

    listener: Listener,
    watcher: Watcher,

    /// Every screen should be redrawn at the next opportunity, whatever the
    /// timers say. Set by a configure, a config change, or a control request.
    dirty: bool,
    pub(crate) exit: bool,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("screens", &self.screens)
            .field("config_path", &self.config_path)
            .field("dirty", &self.dirty)
            .finish_non_exhaustive()
    }
}

impl App {
    pub(crate) fn new(
        globals: &GlobalList,
        qh: &QueueHandle<Self>,
        config_paths: Vec<PathBuf>,
        user_config_path: Option<PathBuf>,
        listener: Listener,
    ) -> Result<Self> {
        let compositor =
            CompositorState::bind(globals, qh).context("the compositor has no wl_compositor")?;
        let layer_shell = LayerShell::bind(globals, qh).context(
            "the compositor does not implement wlr-layer-shell, which a wallpaper needs",
        )?;
        let shm = Shm::bind(globals, qh).context("the compositor has no wl_shm")?;

        let loaded = config::load_from(&config_paths, report_config_error);
        match &loaded.path {
            Some(path) => tracing::info!(path = %path.display(), "configuration"),
            None => tracing::info!("no canvas.toml found; using the defaults"),
        }

        let names_background = loaded.names_background();
        let engine = Engine::new(loaded.background(), loaded.config.render, Instant::now());
        tracing::info!(background = %engine.background().describe(), "starting");

        let mut app = Self {
            registry_state: RegistryState::new(globals),
            output_state: OutputState::new(globals, qh),
            compositor,
            layer_shell,
            shm,
            engine,
            screens: Vec::new(),
            config_paths,
            config_path: loaded.path,
            user_config_path,
            config_names_background: names_background,
            listener,
            watcher: Watcher::new()?,
            dirty: true,
            exit: false,
        };
        app.rewatch();
        Ok(app)
    }

    /// Descriptors the event loop should wake on: the control socket and
    /// inotify.
    ///
    /// Duplicates, so the loop can own what it polls while this keeps what it
    /// reads. Draining both happens in [`App::settle`] rather than in the
    /// loop's callbacks, so that the decision to redraw is taken in one place
    /// with everything that changed already applied.
    pub(crate) fn event_fds(&self) -> Result<[std::os::fd::OwnedFd; 2]> {
        Ok([
            self.listener
                .try_clone_fd()
                .context("cannot duplicate the control socket's descriptor")?,
            self.watcher
                .try_clone_fd()
                .context("cannot duplicate the inotify descriptor")?,
        ])
    }

    /// How long the event loop may sleep.
    ///
    /// `None` means "until a descriptor says something", which is the answer
    /// for a static wallpaper and the reason this daemon costs nothing when it
    /// is not animating.
    pub(crate) fn next_wake(&self) -> Option<Duration> {
        if self.dirty && self.screens.iter().any(Screen::is_ready) {
            return Some(Duration::ZERO);
        }
        self.engine.next_wake(Instant::now())
    }

    /// Everything that happens after the event loop has dispatched.
    pub(crate) fn settle(&mut self, qh: &QueueHandle<Self>) {
        self.drain_control();
        self.drain_watches();

        let now = Instant::now();
        if self.engine.poll(now) {
            self.dirty = true;
        }
        if self.dirty {
            self.redraw(qh, now);
        }
    }

    /// Draw every screen that is ready for a frame.
    fn redraw(&mut self, qh: &QueueHandle<Self>, now: Instant) {
        let detail = self.engine.render().detail;
        let mut drawn = 0;
        let mut waiting = false;

        {
            // Scoped, so `frame`'s borrow of the engine ends before the frame
            // count is written back to it.
            let frame = self.engine.frame(now);
            for screen in &mut self.screens {
                if !screen.is_ready() {
                    // Either not configured yet, or the compositor has not
                    // asked for another frame. Both mean "later", not "never".
                    waiting = true;
                    continue;
                }
                if screen.draw(&frame, detail, qh) {
                    drawn += 1;
                }
            }
        }

        if drawn > 0 {
            self.engine.note_frame(now, drawn);
        }
        // The flag is only cleared once every screen has actually had the
        // frame. A screen still waiting on a callback gets it on the next pass
        // rather than missing this change entirely.
        self.dirty = waiting;
    }

    // -- the control socket -------------------------------------------------

    /// Serve every connection that is waiting.
    fn drain_control(&mut self) {
        while let Some(mut stream) = self.listener.accept() {
            let response = match control::read_request(&mut stream) {
                Ok(request) => {
                    tracing::debug!(?request, "control");
                    self.handle(request)
                }
                Err(e) => {
                    // Logged rather than answered: whatever is on the other end
                    // did not speak this protocol, so there is no reason to
                    // think it would understand the reply.
                    tracing::warn!("ignoring a control connection: {e:#}");
                    continue;
                }
            };
            if let Err(e) = control::write_response(&mut stream, &response) {
                tracing::warn!("cannot answer a control request: {e:#}");
            }
        }
    }

    fn handle(&mut self, request: Request) -> Response {
        match request {
            Request::Status => Response::Status(Box::new(self.status())),
            Request::Scenes => Response::Scenes {
                scenes: raven_scene::Kind::ALL
                    .into_iter()
                    .map(|kind| SceneInfo {
                        name: kind.name().to_string(),
                        summary: kind.summary().to_string(),
                    })
                    .collect(),
            },
            Request::Reload => {
                self.reload();
                Response::Ok {
                    message: match &self.config_path {
                        Some(path) => format!("reloaded {}", path.display()),
                        None => "no canvas.toml found; using the defaults".to_string(),
                    },
                }
            }
            Request::Advance { by } => {
                if self.engine.advance(by, Instant::now()) {
                    self.dirty = true;
                    Response::Ok {
                        message: match self.engine.current_image() {
                            Some(path) => format!("showing {}", path.display()),
                            None => "advanced".to_string(),
                        },
                    }
                } else {
                    Response::Failed {
                        message: "there is no slideshow to advance".to_string(),
                    }
                }
            }
            Request::SetPaused { paused } => {
                let changed = self.engine.set_paused(paused, Instant::now());
                if changed {
                    self.dirty = true;
                }
                Response::Ok {
                    message: match (paused, changed) {
                        (true, true) => "paused".to_string(),
                        (false, true) => "resumed".to_string(),
                        (true, false) => "already paused".to_string(),
                        (false, false) => "already running".to_string(),
                    },
                }
            }
            Request::Apply {
                background,
                persist,
            } => self.apply(background, persist),
        }
    }

    /// Show something else, and optionally write it down.
    fn apply(&mut self, background: Background, persist: bool) -> Response {
        let description = background.describe();

        // Persisting happens *after* the engine has accepted it, so a request
        // naming a scene that does not exist cannot leave a config file behind
        // that the daemon will refuse to load on every future start.
        let render = self.engine.render();
        if let Err(e) = self
            .engine
            .apply(background.clone(), render, Instant::now())
        {
            return Response::Failed {
                message: format!("{e:#}"),
            };
        }
        self.dirty = true;
        self.rewatch();

        if !persist {
            return Response::Ok {
                message: format!("showing {description}"),
            };
        }

        let Some(path) = self.user_config_path.clone() else {
            return Response::Failed {
                message: format!(
                    "showing {description}, but there is no home directory to write a config file into"
                ),
            };
        };

        let config = Config {
            background: Some(background),
            render,
        };
        match config::save(&path, &config) {
            Ok(()) => {
                // The write wakes our own watcher, which reloads the file and
                // finds it identical -- `Engine::apply` short-circuits that.
                self.config_path = Some(path.clone());
                Response::Ok {
                    message: format!("showing {description}, saved to {}", path.display()),
                }
            }
            Err(e) => Response::Failed {
                message: format!("showing {description}, but cannot save it: {e:#}"),
            },
        }
    }

    fn status(&self) -> Status {
        Status {
            background: self.engine.background().clone(),
            config_path: self.config_path.clone(),
            current_image: self.engine.current_image().map(Path::to_path_buf),
            playlist: self.engine.playlist_len(),
            paused: self.engine.is_paused(),
            animated: self.engine.is_animated(),
            frames: self.engine.frames(),
            outputs: self
                .screens
                .iter()
                .map(|screen| {
                    let (width, height) = screen.size();
                    OutputStatus {
                        name: screen.name().to_string(),
                        width,
                        height,
                        scale: screen.scale(),
                    }
                })
                .collect(),
        }
    }

    // -- the filesystem -----------------------------------------------------

    /// Act on everything inotify reported.
    fn drain_watches(&mut self) {
        let changed = self.watcher.drain();
        if changed.is_empty() {
            return;
        }

        // A config file changed: re-read, which is also the debounce. Several
        // events from one save all resolve to the same file contents, and
        // `Engine::apply` treats an identical background as a no-op.
        if changed
            .iter()
            .any(|path| self.config_paths.iter().any(|candidate| candidate == path))
        {
            tracing::info!("the configuration changed on disk");
            self.reload();
            return;
        }

        // The machine's wallpaper changed under a desktop that is showing it.
        // Reloading rather than rescanning: `Loaded::background` reads `set/`
        // again, so this picks up a new file, a changed symlink and a removed
        // one alike, and falls back through the same order it did at startup.
        if !self.config_names_background
            && changed
                .iter()
                .any(|path| path.starts_with(installed::set_dir()))
        {
            tracing::info!("the wallpaper this machine has set changed");
            self.reload();
            return;
        }

        // Otherwise it was the slideshow directory.
        if self.engine.rescan_directory() {
            self.dirty = true;
        }
    }

    fn reload(&mut self) {
        let loaded = config::load_from(&self.config_paths, report_config_error);
        self.config_path = loaded.path.clone();
        self.config_names_background = loaded.names_background();

        let before = self.engine.background().clone();
        if let Err(e) = self
            .engine
            .apply(loaded.background(), loaded.config.render, Instant::now())
        {
            tracing::warn!("keeping the current wallpaper: {e:#}");
            return;
        }

        if *self.engine.background() != before {
            tracing::info!(background = %self.engine.background().describe(), "now showing");
            self.dirty = true;
        }
        self.rewatch();
    }

    /// Watch the directories that matter for what is configured now.
    fn rewatch(&mut self) {
        let mut directories = watch::directories_of(&self.config_paths);
        if let Background::Slideshow { directory, .. } = self.engine.background() {
            directories.push(directory.clone());
        }
        // The machine's wallpaper, but only while nothing is overriding it.
        // `set/` usually does not exist, and `Watcher::watch` skips a directory
        // it cannot watch after saying so once -- which is the right outcome
        // here rather than something to guard against, because a machine that
        // creates `set/` later is a machine that has just set its first
        // wallpaper, and the *parent* is watched too so that create is seen.
        if !self.config_names_background {
            directories.push(installed::set_dir());
            if let Some(parent) = installed::set_dir().parent() {
                directories.push(parent.to_path_buf());
            }
        }
        directories.sort();
        directories.dedup();
        self.watcher.watch(&directories);
    }

    // -- outputs ------------------------------------------------------------

    fn add_output(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.screens.iter().any(|screen| *screen.output() == output) {
            return;
        }

        let info = self.output_state.info(&output);
        let name = screen::output_name(
            info.as_ref().and_then(|info| info.name.as_deref()),
            info.as_ref().and_then(|info| info.description.as_deref()),
            info.as_ref().map_or(0, |info| info.id),
        );

        match Screen::new(
            &self.compositor,
            &self.layer_shell,
            &self.shm,
            qh,
            output,
            name.clone(),
        ) {
            Ok(mut screen) => {
                if let Some(info) = &info {
                    screen.set_scale(info.scale_factor);
                }
                tracing::info!(output = %name, "drawing the wallpaper here");
                self.screens.push(screen);
                self.dirty = true;
            }
            Err(e) => tracing::error!(output = %name, "cannot create a wallpaper surface: {e:#}"),
        }
    }

    fn remove_output(&mut self, output: &wl_output::WlOutput) {
        self.screens.retain(|screen| {
            let keep = screen.output() != output;
            if !keep {
                tracing::info!(output = %screen.name(), "gone");
            }
            keep
        });
    }
}

/// One place to say what a bad config file looks like in the log.
fn report_config_error(path: &Path, error: anyhow::Error) {
    tracing::warn!(
        path = %path.display(),
        "ignoring this configuration file: {error:#}"
    );
}

// ---------------------------------------------------------------------------
// Wayland
// ---------------------------------------------------------------------------

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        for screen in &mut self.screens {
            if screen.owns(surface) && screen.set_scale(factor) {
                tracing::debug!(output = %screen.name(), factor, "scale");
                self.dirty = true;
            }
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
        // huginn does not rotate outputs, and a wallpaper anchored to all four
        // edges is reconfigured with the new size if one ever does.
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Only ever bookkeeping. Drawing is decided by `settle`, from the
        // engine's clock -- if this drew directly, the wallpaper would animate
        // at the panel's refresh rate rather than at the configured one.
        for screen in &mut self.screens {
            if screen.owns(surface) {
                screen.frame_done();
            }
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_output(qh, output);
    }

    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        for screen in &mut self.screens {
            if *screen.output() == output && screen.set_scale(info.scale_factor) {
                self.dirty = true;
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.remove_output(&output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor took the surface away. That is not a reason to exit:
        // an output was unplugged, or huginn is restarting its shell, and the
        // remaining screens still want a wallpaper.
        self.screens.retain(|screen| {
            let keep = !screen.matches_layer(layer);
            if !keep {
                tracing::info!(output = %screen.name(), "the compositor closed this surface");
            }
            keep
        });

        if self.screens.is_empty() {
            tracing::info!("no surfaces left; waiting for an output");
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (width, height) = configure.new_size;
        let compositor = &self.compositor;
        let mut resized = false;

        for screen in &mut self.screens {
            if screen.matches_layer(layer) && screen.configured(width, height) {
                tracing::info!(output = %screen.name(), width, height, "configured");
                // The opaque region has to be re-declared at the new size.
                screen.set_opaque(compositor);
                resized = true;
            }
        }

        if resized {
            self.dirty = true;
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(App);
