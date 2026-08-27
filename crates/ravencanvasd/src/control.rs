//! The control socket, as a transport.
//!
//! What a request *means* is decided in `app`; this is the part that binds a
//! socket, accepts a connection and moves bytes. Keeping the two apart is what
//! lets the message handling be exercised without a socket, and the socket
//! handling be read without wading through what every request does.
//!
//! # Stale sockets
//!
//! A Unix socket file outlives the process that made it. If the daemon is
//! killed, the path is still there, and the next daemon cannot bind it.
//!
//! The way to tell a stale socket from a live one is to *connect* to it. A
//! refused connection means nothing is listening and the file is a leftover,
//! which is safe to remove. A successful one means a daemon is already
//! running, which is not an error to work around -- two wallpaper daemons on
//! one session would fight over the background layer -- so this refuses to
//! start and says so.
//!
//! # Blocking, with timeouts
//!
//! The listener is non-blocking, because the event loop polls it. An accepted
//! connection is *not*: a request and its reply are a few hundred bytes and a
//! handful of microseconds, and doing that inline is simpler than a state
//! machine per connection.
//!
//! The timeouts are what make that safe. Without them a client that connects
//! and then says nothing would stop the wallpaper from being drawn for as long
//! as it felt like -- one `nc` away from a frozen desktop.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use raven_canvas_proto::{Request, Response};

/// How long a connected client gets to send its request, or read its reply.
///
/// Generous for something that should take microseconds, and short enough that
/// a wedged client is a log line rather than a stuck daemon.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The mode of the directory the socket lives in.
///
/// The socket's own mode is not set: it is protected by the directory, which
/// is the thing that can be checked and cannot be raced. See the protocol
/// crate's documentation.
const DIRECTORY_MODE: u32 = 0o700;

/// A bound control socket, removed when dropped.
#[derive(Debug)]
pub(crate) struct Listener {
    listener: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Bind `path`, creating its directory and clearing a stale socket.
    pub(crate) fn bind(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            // Best effort: on a runtime directory this is already the mode,
            // and a failure to tighten it is worth a warning rather than a
            // refusal to start.
            if let Err(e) =
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(DIRECTORY_MODE))
            {
                tracing::warn!(
                    directory = %parent.display(),
                    "cannot set the control directory to {DIRECTORY_MODE:o}: {e}"
                );
            }
        }

        clear_stale(path)?;

        let listener =
            UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
        listener
            .set_nonblocking(true)
            .context("cannot make the control socket non-blocking")?;

        tracing::info!(path = %path.display(), "listening for control connections");
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    /// Take the next waiting connection, if there is one.
    ///
    /// `None` means there is nothing pending right now, which is the normal
    /// answer -- the event loop calls this in a loop until it gets one.
    pub(crate) fn accept(&self) -> Option<UnixStream> {
        match self.listener.accept() {
            Ok((stream, _)) => {
                // The listener is non-blocking and an accepted socket inherits
                // nothing; setting this explicitly rather than relying on that
                // is the difference between a timeout and a busy loop.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(TIMEOUT));
                let _ = stream.set_write_timeout(Some(TIMEOUT));
                Some(stream)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(e) => {
                tracing::warn!("cannot accept a control connection: {e}");
                None
            }
        }
    }

    /// A duplicate of the listening descriptor, for the event loop to poll.
    ///
    /// A duplicate rather than a borrow because calloop's `Generic` source
    /// owns what it polls, and this type has to keep the listener to accept
    /// on. Both descriptors refer to the same open file, so readiness on one
    /// is readiness on the other -- and getting one this way needs no
    /// `unsafe`, which `BorrowedFd::borrow_raw` would.
    pub(crate) fn try_clone_fd(&self) -> std::io::Result<std::os::fd::OwnedFd> {
        use std::os::fd::AsFd;
        self.listener.as_fd().try_clone_to_owned()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Best effort. A socket left behind is cleared by the next daemon's
        // `clear_stale`, so failing here costs nothing.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Remove `path` if it is a socket nobody is listening on.
fn clear_stale(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match UnixStream::connect(path) {
        Ok(_) => anyhow::bail!(
            "another ravencanvasd is already listening on {}; two wallpaper daemons would fight over the background layer",
            path.display()
        ),
        Err(_) => {
            tracing::info!(path = %path.display(), "clearing a stale control socket");
            std::fs::remove_file(path)
                .with_context(|| format!("cannot remove the stale socket at {}", path.display()))?;
            Ok(())
        }
    }
}

/// Read one request from a connected client.
pub(crate) fn read_request(stream: &mut UnixStream) -> Result<Request> {
    let mut reader =
        std::io::BufReader::new(stream.try_clone().context("cannot clone the socket")?);
    raven_canvas_proto::read_message(&mut reader).context("cannot read a control request")
}

/// Send one reply.
pub(crate) fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let mut writer = std::io::BufWriter::new(stream);
    raven_canvas_proto::write_message(&mut writer, response).context("cannot send a control reply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use raven_canvas_proto::Background;

    /// A socket path in a scratch directory, removed with the test.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("ravencanvas-control-{name}"));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }

        fn socket(&self) -> PathBuf {
            self.0.join("control.sock")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn binding_creates_the_directory_and_the_socket() {
        let scratch = Scratch::new("bind");
        let _listener = Listener::bind(&scratch.socket()).expect("bind");

        assert!(scratch.socket().exists());
        let mode = std::fs::metadata(&scratch.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, DIRECTORY_MODE, "the directory is not private");
    }

    #[test]
    fn the_socket_is_removed_when_the_listener_is_dropped() {
        let scratch = Scratch::new("drop");
        {
            let _listener = Listener::bind(&scratch.socket()).expect("bind");
            assert!(scratch.socket().exists());
        }
        assert!(
            !scratch.socket().exists(),
            "the socket outlived its listener"
        );
    }

    /// The case this exists for: the daemon was killed and its socket file is
    /// still there. Starting again has to work.
    #[test]
    fn a_stale_socket_is_cleared_rather_than_being_fatal() {
        let scratch = Scratch::new("stale");
        std::fs::create_dir_all(&scratch.0).expect("scratch");

        // Dropping a `UnixListener` closes the descriptor but does *not*
        // unlink the path -- which is exactly what a killed daemon leaves
        // behind, and why this function exists at all.
        drop(UnixListener::bind(scratch.socket()).expect("orphan"));
        assert!(
            scratch.socket().exists(),
            "the orphaned socket file is the premise"
        );

        let listener = Listener::bind(&scratch.socket());
        assert!(listener.is_ok(), "{:?}", listener.err());
    }

    /// The other half: a socket somebody *is* listening on must not be taken
    /// over. Two wallpaper daemons on one session is not a state to recover
    /// into.
    #[test]
    fn a_live_socket_is_refused_rather_than_stolen() {
        let scratch = Scratch::new("live");
        let _first = Listener::bind(&scratch.socket()).expect("first bind");

        let error = Listener::bind(&scratch.socket()).unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("already listening"), "{text}");
    }

    #[test]
    fn accepting_with_nobody_waiting_returns_nothing() {
        let scratch = Scratch::new("empty");
        let listener = Listener::bind(&scratch.socket()).expect("bind");
        assert!(listener.accept().is_none());
    }

    /// The whole round trip, over a real socket: a client connects, sends a
    /// request, the daemon reads it and replies, and the client reads that.
    #[test]
    fn a_request_and_its_reply_cross_a_real_socket() {
        let scratch = Scratch::new("roundtrip");
        let listener = Listener::bind(&scratch.socket()).expect("bind");

        let request = Request::Apply {
            background: Background::Color {
                color: "#7AA2F7".into(),
            },
            persist: true,
        };

        let path = scratch.socket();
        let sent = request.clone();
        let client = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&path).expect("connect");
            raven_canvas_proto::write_message(&mut stream, &sent).expect("send");
            raven_canvas_proto::read_message::<_, Response>(&mut stream).expect("reply")
        });

        // The listener is non-blocking, so spin until the connection lands.
        let mut server = loop {
            if let Some(stream) = listener.accept() {
                break stream;
            }
            std::thread::yield_now();
        };

        assert_eq!(read_request(&mut server).expect("read"), request);
        write_response(
            &mut server,
            &Response::Ok {
                message: "done".into(),
            },
        )
        .expect("write");

        assert_eq!(
            client.join().expect("client"),
            Response::Ok {
                message: "done".into()
            }
        );
    }

    #[test]
    fn a_client_that_says_nothing_is_an_error_rather_than_a_hang() {
        let scratch = Scratch::new("silent");
        let listener = Listener::bind(&scratch.socket()).expect("bind");

        let path = scratch.socket();
        let client = std::thread::spawn(move || {
            let stream = UnixStream::connect(&path).expect("connect");
            // Hold the connection open without sending anything, then go away.
            std::thread::sleep(Duration::from_millis(50));
            drop(stream);
        });

        let mut server = loop {
            if let Some(stream) = listener.accept() {
                break stream;
            }
            std::thread::yield_now();
        };

        // The client closes without writing, so this is a clean truncation
        // rather than a timeout -- which is the same outcome from the
        // daemon's point of view, and much faster to test.
        assert!(read_request(&mut server).is_err());
        client.join().expect("client");
    }
}
