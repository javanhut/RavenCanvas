//! `ravencanvasd` -- the RavenLinux wallpaper daemon.
//!
//! An ordinary, unprivileged Wayland client. It binds `wlr-layer-shell`, puts
//! one surface on the `background` layer of each output, and draws a picture
//! on it. It reads a config file, listens on a Unix socket in the user's
//! runtime directory, and does nothing else. There is no daemon behind it and
//! nothing it can do that the user could not.
//!
//! # Why this is a separate process
//!
//! huginn draws its own dock, launcher and quick settings inside its render
//! loop, because the design spec says the shell is not a client: anything that
//! must feel instant and must never fail does not get to be a separate process
//! that can miss a frame or die.
//!
//! A wallpaper is precisely the case that rule is not about. It is allowed to
//! fail -- the file may be missing, corrupt, or on a disk that is not mounted
//! yet -- and huginn already paints its own background colour under
//! everything, so the worst thing this process's death can do is leave a plain
//! desktop. RavenGUI's `docs/protocols.md` says so directly: *"Panels, the dock
//! and the wallpaper are wlr-layer-shell surfaces. Do not duplicate those
//! here."*
//!
//! # What it costs when it is not doing anything
//!
//! Nothing, and that is a design goal rather than a hope. A still wallpaper --
//! an image, a colour, or a scene at speed zero -- is drawn once and then the
//! process blocks on its descriptors with no timer armed at all. See
//! [`engine::Engine::next_wake`], which returns `None` for exactly those
//! cases, and `screen`'s note on frame callbacks, which is why a wallpaper
//! covered by a full-screen window stops rendering without being told.
//!
//! # Starting it
//!
//! From `raven-wayland-session`, backgrounded before the `exec` of the
//! compositor -- the same shape the session's `dbus-daemon` uses. That means
//! this process starts *before* there is a Wayland socket to connect to, which
//! is why [`connect`] retries rather than exiting: the alternative is a
//! start-order race that this loses on every boot.

#![forbid(unsafe_code)]

mod app;
mod config;
mod control;
mod engine;
mod installed;
mod playlist;
mod preview;
mod resolve;
mod screen;
mod watch;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use calloop::EventLoop;
use calloop::generic::Generic;
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;

use crate::app::App;
use crate::control::Listener;

/// How long to wait for the compositor's Wayland socket, by default.
///
/// The same ten seconds `ravend` gives a compositor to bind, and for the same
/// reason: a compositor that exits before binding is noticed immediately, and
/// this is the ceiling for one that merely takes a while.
const DEFAULT_WAYLAND_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to retry the connection while waiting.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("ravencanvasd: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// What the command line asked for.
#[derive(Debug, Default)]
struct Args {
    /// One file to read instead of searching the usual places.
    config: Option<PathBuf>,
    wayland_timeout: Option<Duration>,
    preview: Option<Vec<String>>,
}

fn usage() -> &'static str {
    "ravencanvasd -- the RavenLinux wallpaper daemon

  --config PATH           read this file instead of searching for canvas.toml
  --wayland-timeout SECS  how long to wait for the compositor's socket (default 10)
  --preview FILE [WxH] [SECONDS] [SCENE]
                          render one frame to a PNG and exit, with no compositor
  -h, --help              this
  -V, --version           the version

Everything else is in canvas.toml, or is said over the control socket with
the `ravencanvas` command."
}

fn parse_args() -> Result<Option<Args>> {
    let mut args = Args::default();
    let mut argv = std::env::args().skip(1);

    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("ravencanvasd {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--config" => {
                let path = argv.next().context("--config needs a path")?;
                args.config = Some(PathBuf::from(path));
            }
            "--wayland-timeout" => {
                let seconds: u64 = argv
                    .next()
                    .context("--wayland-timeout needs a number of seconds")?
                    .parse()
                    .context("--wayland-timeout takes a number of seconds")?;
                args.wayland_timeout = Some(Duration::from_secs(seconds));
            }
            // Takes the rest of the command line, so a scene name cannot be
            // mistaken for a flag.
            "--preview" => {
                args.preview = Some(argv.by_ref().collect());
                break;
            }
            other => bail!("unknown option {other:?}\n\n{}", usage()),
        }
    }
    Ok(Some(args))
}

fn run() -> Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };

    // Where the configuration is. `--config` replaces the search entirely,
    // which is what makes it usable for testing a file before installing it.
    let (config_paths, user_config_path) = match &args.config {
        Some(path) => (vec![path.clone()], Some(path.clone())),
        None => (
            config::default_search_paths(),
            config::user_path(
                std::env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .as_deref(),
                std::env::var_os("HOME").map(PathBuf::from).as_deref(),
            ),
        ),
    };

    if let Some(preview) = &args.preview {
        let options = preview::parse(preview)?;
        return preview::run(&options, &config_paths);
    }

    // The socket is bound before the compositor is reached, so that a second
    // daemon started by accident fails immediately with "already listening"
    // rather than after spending the connection timeout waiting for a Wayland
    // socket it was never going to get to use.
    let socket = raven_canvas_proto::socket_path().context(
        "there is no $XDG_RUNTIME_DIR to put a control socket in; is this running in a session?",
    )?;
    let listener = Listener::bind(&socket)?;

    let connection = connect(args.wayland_timeout.unwrap_or(DEFAULT_WAYLAND_TIMEOUT))?;
    let (globals, queue) =
        registry_queue_init(&connection).context("cannot initialize the Wayland registry")?;
    let qh = queue.handle();

    let mut app = App::new(&globals, &qh, config_paths, user_config_path, listener)?;

    let mut event_loop: EventLoop<App> =
        EventLoop::try_new().context("cannot create an event loop")?;
    let handle = event_loop.handle();

    WaylandSource::new(connection.clone(), queue)
        .insert(handle.clone())
        .map_err(|e| anyhow::anyhow!("cannot watch the Wayland connection: {e}"))?;

    // The control socket and inotify. Both callbacks do nothing: waking the
    // loop is the whole job, and both are drained in `App::settle`, where the
    // decision to redraw is taken once with everything that changed already
    // applied. Level-triggered, so a descriptor that still has something on it
    // wakes the loop again rather than being missed.
    for descriptor in app.event_fds()? {
        handle
            .insert_source(
                Generic::new(descriptor, calloop::Interest::READ, calloop::Mode::Level),
                |_, _, _| Ok(calloop::PostAction::Continue),
            )
            .map_err(|e| anyhow::anyhow!("cannot watch a descriptor: {e}"))?;
    }

    tracing::info!("ravencanvasd {} is up", env!("CARGO_PKG_VERSION"));

    while !app.exit {
        // Recomputed every iteration rather than held in a timer source. A
        // config change picked up in this dispatch affects the very next
        // sleep, and a still wallpaper produces `None` -- which blocks the
        // process on its descriptors with nothing armed at all.
        let timeout = app.next_wake();

        if let Err(e) = event_loop.dispatch(timeout, &mut app) {
            // The usual cause is the compositor going away, which is the
            // session ending. It is not an error worth a failure exit code.
            tracing::info!("the Wayland connection ended: {e}");
            break;
        }

        app.settle(&qh);

        // `WaylandSource` flushes before it sleeps, so this is belt and
        // braces -- but `settle` is where the commits happen, and a frame that
        // sits in the buffer until the next wakeup is a frame late.
        if let Err(e) = connection.flush() {
            tracing::info!("the Wayland connection ended: {e}");
            break;
        }
    }

    tracing::info!("ravencanvasd exiting");
    Ok(())
}

/// Connect to the compositor, waiting for it if it is not up yet.
///
/// This process is started from `raven-wayland-session` *before* the
/// compositor is executed, so at the moment it runs there is usually no socket
/// to connect to. Exiting on the first failure would mean losing a start-order
/// race on every boot; retrying until a deadline turns that race into a
/// non-event.
///
/// A missing `$WAYLAND_DISPLAY` is not retried. That is not a race, it is a
/// wallpaper daemon started outside a session, and waiting ten seconds to say
/// so helps nobody.
fn connect(timeout: Duration) -> Result<Connection> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none_or(|value| value.is_empty()) {
        bail!("WAYLAND_DISPLAY is not set; ravencanvasd is a Wayland client and needs a session");
    }

    let deadline = Instant::now() + timeout;
    let mut waited = false;
    loop {
        match Connection::connect_to_env() {
            Ok(connection) => {
                if waited {
                    tracing::info!("the compositor is up");
                }
                return Ok(connection);
            }
            Err(e) if Instant::now() >= deadline => {
                return Err(anyhow::Error::new(e).context(format!(
                    "no Wayland compositor answered within {}s",
                    timeout.as_secs()
                )));
            }
            Err(_) => {
                if !waited {
                    tracing::info!(
                        "waiting up to {}s for the compositor's Wayland socket",
                        timeout.as_secs()
                    );
                    waited = true;
                }
                std::thread::sleep(RETRY_INTERVAL);
            }
        }
    }
}
