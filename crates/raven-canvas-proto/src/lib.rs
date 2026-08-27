//! What `ravencanvasd` and the `ravencanvas` CLI say to each other.
//!
//! A 4-byte big-endian length, then that many bytes of JSON. The same framing
//! RavenLogin's greeter protocol uses, and for the same reasons: the message
//! rate is "a few per session", being able to read the traffic with `socat`
//! while bringing the thing up is worth more than the bytes, and a length
//! prefix that means different things on different machines is a bug waiting
//! for the first big-endian port.
//!
//! # This crate names things but does not understand them
//!
//! A colour crosses this socket as `"#7AA2F7"` and a scene as `"aurora"`,
//! rather than as the types `raven_paint::Color` and `raven_scene::Kind` that
//! they will become. That is deliberate, and it is not laziness about deriving
//! `Serialize`:
//!
//! - **There is one parser.** A colour arriving from the config file and a
//!   colour arriving from the CLI go through exactly the same code in the
//!   daemon, so they cannot disagree about what `#7AF` means, and the error
//!   message when one is wrong is written once.
//! - **The validation happens where the answer can be sent back.** A CLI that
//!   parsed colours itself would reject `#GGGGGG` locally with a different
//!   message than a config file gets, and would still have to handle the
//!   daemon rejecting something it thought was fine.
//! - **This crate stays cheap.** The CLI links it and almost nothing else, and
//!   nothing here pulls in an image decoder or a renderer.
//!
//! # What this socket is protected by
//!
//! The directory it lives in, and nothing else. [`socket_path`] puts it under
//! `$XDG_RUNTIME_DIR`, which the kernel and the session manager have already
//! made `0700` and owned by the user; a second identity check inside the
//! daemon could not fail if that is true and would not help if it is not.
//!
//! This is the one place `ravencanvasd` differs from `ravend`, which puts its
//! socket in `/run/raven-login` and creates that directory itself. `ravend` is
//! a system daemon serving a login screen that has not decided who the user
//! is yet. This is a session daemon; the user is already known, and it is
//! theirs.

#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The directory, under `$XDG_RUNTIME_DIR`, that the socket lives in.
pub const RUNTIME_SUBDIR: &str = "raven-canvas";

/// The socket's filename inside [`RUNTIME_SUBDIR`].
pub const SOCKET_NAME: &str = "control.sock";

/// The environment variable naming the per-user runtime directory.
pub const RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";

/// An override for the whole socket path, for tests and for running two
/// daemons against two nested compositors at once.
pub const SOCKET_VAR: &str = "RAVEN_CANVAS_SOCKET";

/// The largest message this protocol will read.
///
/// Small on purpose. The biggest legitimate message is a status reply listing
/// a handful of outputs and a playlist path, which is well under a kilobyte;
/// the cap exists so that a length prefix corrupted to 4 GiB makes the daemon
/// close a connection instead of asking the allocator for 4 GiB.
pub const MAX_MESSAGE: usize = 256 * 1024;

/// Where the control socket is.
///
/// `$RAVEN_CANVAS_SOCKET` wins if it is set, so a nested development session
/// can run its own daemon without fighting the real one for the path.
/// Otherwise `$XDG_RUNTIME_DIR/raven-canvas/control.sock`.
///
/// Returns `None` when there is no runtime directory to put it in, which
/// happens in exactly one situation worth naming: something started this
/// outside a session. The caller should say so rather than inventing a path in
/// `/tmp`, because a control socket in a world-writable directory is a
/// different thing from one in `$XDG_RUNTIME_DIR` and should not be reached by
/// falling back.
#[must_use]
pub fn socket_path() -> Option<PathBuf> {
    resolve_socket_path(
        std::env::var_os(SOCKET_VAR).as_deref(),
        std::env::var_os(RUNTIME_DIR_VAR).as_deref(),
    )
}

/// [`socket_path`] with the environment passed in.
///
/// Split out so the rules above can be tested without a test mutating
/// process-wide state -- which, on this edition, is `unsafe`, which this
/// workspace forbids, and which would be a race between test threads even if
/// it were not. A function that reads the environment is hard to test; a
/// function that is *given* the environment is not.
#[must_use]
pub fn resolve_socket_path(
    socket_override: Option<&std::ffi::OsStr>,
    runtime_dir: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    // An empty override is not an override; it is a variable somebody
    // exported and left blank, and honouring it would produce an unopenable
    // empty path.
    if let Some(explicit) = socket_override.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let runtime = runtime_dir.filter(|value| !value.is_empty())?;
    Some(Path::new(runtime).join(RUNTIME_SUBDIR).join(SOCKET_NAME))
}

// ---------------------------------------------------------------------------
// What to draw
// ---------------------------------------------------------------------------

/// A background, named rather than resolved.
///
/// This is the one type that crosses every boundary in the project: it is what
/// the config file deserializes to, what the CLI sends, and what a status
/// reply carries back. Keeping a single definition is what stops
/// `ravencanvas set` and a hand-edited `canvas.toml` from being able to
/// express different things.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum Background {
    /// One flat colour. The cheapest thing this daemon can do, and what it
    /// falls back to when everything else has failed.
    Color {
        /// `#RGB`, `#RRGGBB` or `#AARRGGBB`.
        color: String,
    },
    /// One image file.
    Image {
        path: PathBuf,
        /// `cover`, `contain`, `stretch`, `center` or `tile`.
        #[serde(default = "default_fit")]
        fit: String,
        /// Shown wherever the image does not reach, under `contain` and
        /// `center`.
        #[serde(default = "default_color")]
        background: String,
    },
    /// Every image in a directory, in turn.
    Slideshow {
        directory: PathBuf,
        /// How long each image is shown, in seconds.
        #[serde(default = "default_interval")]
        interval: u64,
        /// Whether to visit them in a shuffled order rather than by name.
        #[serde(default)]
        shuffle: bool,
        /// How long to fade between them, in milliseconds. Zero cuts.
        #[serde(default = "default_crossfade")]
        crossfade: u64,
        #[serde(default = "default_fit")]
        fit: String,
        #[serde(default = "default_color")]
        background: String,
    },
    /// A procedural scene.
    Scene {
        /// `gradient`, `aurora`, `plasma` or `starfield`.
        name: String,
        /// Multiplies the rate of everything in the scene. Zero draws one
        /// frame and stops.
        #[serde(default = "default_speed")]
        speed: f32,
        /// Colour stops. Empty means the scene's own defaults.
        #[serde(default)]
        palette: Vec<String>,
    },
}

fn default_fit() -> String {
    "cover".to_string()
}

fn default_color() -> String {
    // huginn's `BACKGROUND`, so an unconfigured letterbox matches the desktop
    // rather than being black.
    "#16161F".to_string()
}

const fn default_interval() -> u64 {
    900
}

const fn default_crossfade() -> u64 {
    800
}

const fn default_speed() -> f32 {
    1.0
}

impl Default for Background {
    fn default() -> Self {
        Self::Scene {
            name: "gradient".to_string(),
            speed: default_speed(),
            palette: Vec::new(),
        }
    }
}

impl Background {
    /// The word `mode = ` takes in the config file.
    #[must_use]
    pub const fn mode(&self) -> &'static str {
        match self {
            Self::Color { .. } => "color",
            Self::Image { .. } => "image",
            Self::Slideshow { .. } => "slideshow",
            Self::Scene { .. } => "scene",
        }
    }

    /// One line describing this background, for `ravencanvas status`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Color { color } => format!("colour {color}"),
            Self::Image { path, fit, .. } => format!("{} ({fit})", path.display()),
            Self::Slideshow {
                directory,
                interval,
                shuffle,
                ..
            } => format!(
                "slideshow of {} every {interval}s{}",
                directory.display(),
                if *shuffle { ", shuffled" } else { "" }
            ),
            Self::Scene { name, speed, .. } => {
                if *speed == 0.0 {
                    format!("scene {name}, frozen")
                } else {
                    format!("scene {name} at {speed}x")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// CLI to daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "kebab-case")]
pub enum Request {
    /// What is on screen.
    Status,
    /// Which scenes this build has.
    Scenes,
    /// Show this instead.
    Apply {
        background: Background,
        /// Whether to write it into the user's config file as well, so it
        /// survives a restart. Without this the change lasts until the daemon
        /// exits or the config file changes underneath it.
        #[serde(default)]
        persist: bool,
    },
    /// Re-read the config file, discarding anything set over this socket.
    Reload,
    /// Move a slideshow on, or back. Ignored by every other mode.
    Advance {
        /// Negative goes backwards.
        by: i32,
    },
    /// Stop or restart animation and slideshow timers.
    ///
    /// A paused daemon still redraws when a screen is resized -- pausing is
    /// about time passing, not about the wallpaper disappearing.
    SetPaused { paused: bool },
}

/// Daemon to CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "kebab-case")]
pub enum Response {
    /// It worked. `message` is written for a person to read.
    Ok {
        message: String,
    },
    /// It did not. `message` is already safe and specific enough to print.
    Failed {
        message: String,
    },
    Status(Box<Status>),
    Scenes {
        scenes: Vec<SceneInfo>,
    },
}

/// What the daemon is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// What is configured.
    pub background: Background,
    /// Which file it was last read from, if any.
    pub config_path: Option<PathBuf>,
    /// The image currently on screen, when there is a file behind it.
    pub current_image: Option<PathBuf>,
    /// How many images the slideshow found. Zero in every other mode.
    pub playlist: usize,
    /// Whether timers are stopped.
    pub paused: bool,
    /// Whether anything is expected to change without being asked to. A
    /// static image, or a scene at speed zero, is `false` -- and a `false`
    /// here means the daemon is genuinely idle rather than merely quiet.
    pub animated: bool,
    /// Frames committed since start, across every screen. The number to look
    /// at when the question is "is this thing costing me anything".
    pub frames: u64,
    /// The screens it is drawing on.
    pub outputs: Vec<OutputStatus>,
}

/// One screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputStatus {
    /// The connector name, when the compositor gave one: `eDP-1`, `HDMI-A-1`.
    pub name: String,
    /// The surface size in device pixels, which is what is actually drawn.
    pub width: i32,
    pub height: i32,
    /// The integer scale the compositor asked for.
    pub scale: i32,
}

/// One scene this build knows how to draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneInfo {
    pub name: String,
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// Something went wrong on the socket.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("the connection closed part-way through a message")]
    Truncated,
    #[error("a message claimed to be {size} bytes, past the {MAX_MESSAGE}-byte limit")]
    TooLarge { size: usize },
    #[error("a message could not be encoded: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("a message could not be understood: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("the socket failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Write one length-prefixed message and flush it.
pub fn write_message<W: Write, T: Serialize>(
    writer: &mut W,
    message: &T,
) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(message).map_err(ProtocolError::Encode)?;
    if body.len() > MAX_MESSAGE {
        return Err(ProtocolError::TooLarge { size: body.len() });
    }

    // The length is checked against the cap before the cast, so this cannot
    // truncate: `MAX_MESSAGE` is far below `u32::MAX`.
    let length = body.len() as u32;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Read one length-prefixed message.
pub fn read_message<R: Read, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<T, ProtocolError> {
    let mut header = [0u8; 4];
    read_exact(reader, &mut header)?;

    let size = u32::from_be_bytes(header) as usize;
    if size > MAX_MESSAGE {
        return Err(ProtocolError::TooLarge { size });
    }

    // Allocated only after the cap has been checked. This is the whole reason
    // `MAX_MESSAGE` exists.
    let mut body = vec![0u8; size];
    read_exact(reader, &mut body)?;
    serde_json::from_slice(&body).map_err(ProtocolError::Decode)
}

/// `Read::read_exact`, but a clean end-of-stream is [`ProtocolError::Truncated`]
/// rather than an `io::Error` the caller has to inspect a kind on.
fn read_exact<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<(), ProtocolError> {
    match reader.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(ProtocolError::Truncated),
        Err(e) => Err(ProtocolError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(message: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let mut buffer = Vec::new();
        write_message(&mut buffer, message).expect("write");
        read_message(&mut &buffer[..]).expect("read")
    }

    #[test]
    fn every_request_survives_the_wire() {
        let requests = [
            Request::Status,
            Request::Scenes,
            Request::Reload,
            Request::Advance { by: -1 },
            Request::SetPaused { paused: true },
            Request::Apply {
                background: Background::Color {
                    color: "#7AA2F7".into(),
                },
                persist: true,
            },
        ];
        for request in requests {
            assert_eq!(roundtrip(&request), request);
        }
    }

    #[test]
    fn every_background_survives_the_wire() {
        let backgrounds = [
            Background::default(),
            Background::Color {
                color: "#16161F".into(),
            },
            Background::Image {
                path: "/usr/share/backgrounds/raven/default.png".into(),
                fit: "contain".into(),
                background: "#000000".into(),
            },
            Background::Slideshow {
                directory: "/home/someone/Pictures".into(),
                interval: 300,
                shuffle: true,
                crossfade: 0,
                fit: "cover".into(),
                background: "#16161F".into(),
            },
            Background::Scene {
                name: "aurora".into(),
                speed: 0.5,
                palette: vec!["#0A0A12".into(), "#7AA2F7".into()],
            },
        ];
        for background in backgrounds {
            assert_eq!(roundtrip(&background), background);
        }
    }

    #[test]
    fn every_response_survives_the_wire() {
        let responses = [
            Response::Ok {
                message: "done".into(),
            },
            Response::Failed {
                message: "no".into(),
            },
            Response::Scenes {
                scenes: vec![SceneInfo {
                    name: "plasma".into(),
                    summary: "waves".into(),
                }],
            },
            Response::Status(Box::new(Status {
                background: Background::default(),
                config_path: Some("/etc/raven/canvas.toml".into()),
                current_image: None,
                playlist: 0,
                paused: false,
                animated: true,
                frames: 12_345,
                outputs: vec![OutputStatus {
                    name: "eDP-1".into(),
                    width: 1920,
                    height: 1080,
                    scale: 1,
                }],
            })),
        ];
        for response in responses {
            assert_eq!(roundtrip(&response), response);
        }
    }

    /// The config file leans on these defaults: `mode = "image"` with nothing
    /// but a path has to mean something sensible.
    #[test]
    fn a_background_fills_in_what_was_left_out() {
        let parsed: Background =
            serde_json::from_str(r#"{"mode":"image","path":"/tmp/a.png"}"#).unwrap();
        assert_eq!(
            parsed,
            Background::Image {
                path: "/tmp/a.png".into(),
                fit: "cover".into(),
                background: "#16161F".into(),
            }
        );
    }

    #[test]
    fn a_slideshow_fills_in_what_was_left_out() {
        let parsed: Background =
            serde_json::from_str(r#"{"mode":"slideshow","directory":"/pics"}"#).unwrap();
        let Background::Slideshow {
            interval,
            crossfade,
            shuffle,
            ..
        } = parsed
        else {
            panic!("not a slideshow");
        };
        assert_eq!((interval, crossfade, shuffle), (900, 800, false));
    }

    #[test]
    fn the_mode_word_matches_the_serialized_tag() {
        for background in [
            Background::Color {
                color: "#000".into(),
            },
            Background::Image {
                path: "/a".into(),
                fit: "cover".into(),
                background: "#000".into(),
            },
            Background::Slideshow {
                directory: "/a".into(),
                interval: 1,
                shuffle: false,
                crossfade: 0,
                fit: "cover".into(),
                background: "#000".into(),
            },
            Background::Scene {
                name: "plasma".into(),
                speed: 1.0,
                palette: vec![],
            },
        ] {
            let json = serde_json::to_value(&background).unwrap();
            assert_eq!(json["mode"], background.mode());
            assert!(!background.describe().is_empty());
        }
    }

    #[test]
    fn a_frozen_scene_says_so_rather_than_saying_zero_x() {
        let frozen = Background::Scene {
            name: "plasma".into(),
            speed: 0.0,
            palette: vec![],
        };
        assert!(
            frozen.describe().contains("frozen"),
            "{}",
            frozen.describe()
        );
    }

    // -- framing ------------------------------------------------------------

    #[test]
    fn a_truncated_stream_is_an_error_rather_than_a_hang() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &Request::Status).unwrap();
        buffer.truncate(buffer.len() - 1);

        let error = read_message::<_, Request>(&mut &buffer[..]).unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated), "{error}");
    }

    #[test]
    fn an_empty_stream_is_an_error_rather_than_an_empty_message() {
        let error = read_message::<_, Request>(&mut &[][..]).unwrap_err();
        assert!(matches!(error, ProtocolError::Truncated), "{error}");
    }

    /// The cap must be enforced on the *prefix*, before anything is
    /// allocated. A length of 4 GiB has to be refused, not attempted.
    #[test]
    fn an_absurd_length_prefix_is_refused_before_allocating() {
        let mut buffer = u32::MAX.to_be_bytes().to_vec();
        buffer.extend_from_slice(b"{}");

        let error = read_message::<_, Request>(&mut &buffer[..]).unwrap_err();
        assert!(
            matches!(error, ProtocolError::TooLarge { size } if size == u32::MAX as usize),
            "{error}"
        );
    }

    #[test]
    fn a_message_that_is_not_this_protocol_is_an_error() {
        let mut buffer = 2u32.to_be_bytes().to_vec();
        buffer.extend_from_slice(b"[]");

        let error = read_message::<_, Request>(&mut &buffer[..]).unwrap_err();
        assert!(matches!(error, ProtocolError::Decode(_)), "{error}");
    }

    #[test]
    fn several_messages_can_share_a_stream() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &Request::Status).unwrap();
        write_message(&mut buffer, &Request::Reload).unwrap();

        let mut cursor = &buffer[..];
        assert_eq!(
            read_message::<_, Request>(&mut cursor).unwrap(),
            Request::Status
        );
        assert_eq!(
            read_message::<_, Request>(&mut cursor).unwrap(),
            Request::Reload
        );
    }

    // -- the socket path ----------------------------------------------------

    fn os(value: &str) -> &std::ffi::OsStr {
        std::ffi::OsStr::new(value)
    }

    #[test]
    fn the_socket_lives_under_the_runtime_directory() {
        assert_eq!(
            resolve_socket_path(None, Some(os("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/raven-canvas/control.sock"))
        );
    }

    #[test]
    fn an_explicit_socket_wins() {
        assert_eq!(
            resolve_socket_path(Some(os("/tmp/nested.sock")), Some(os("/run/user/1000"))),
            Some(PathBuf::from("/tmp/nested.sock"))
        );
    }

    #[test]
    fn an_empty_variable_is_not_a_value() {
        assert_eq!(
            resolve_socket_path(Some(os("")), Some(os("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/raven-canvas/control.sock"))
        );
        assert_eq!(resolve_socket_path(Some(os("")), Some(os(""))), None);
    }

    /// No runtime directory means no path. Falling back to `/tmp` would put a
    /// control socket somewhere any account on the machine can reach, which is
    /// a different thing from `$XDG_RUNTIME_DIR` and must not be arrived at by
    /// accident.
    #[test]
    fn no_runtime_directory_means_no_path_rather_than_a_temporary_one() {
        assert_eq!(resolve_socket_path(None, None), None);
    }
}
