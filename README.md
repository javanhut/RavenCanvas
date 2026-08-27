# RavenCanvas

The wallpaper daemon for [RavenLinux](../RavenLinux). An ordinary unprivileged
Wayland client that puts one `wlr-layer-shell` surface on the background layer
of each output and draws a picture on it.

| Binary | What it is |
|---|---|
| `ravencanvasd` | the daemon: one layer surface per output, a config file, a control socket |
| `ravencanvas` | the CLI that talks to it |

```bash
imlazy build            # cargo build --release
sudo imlazy install     # into /usr, and started with your session
imlazy preview          # one frame to a PNG, no compositor needed
```

## The picture it draws

Four modes, and one rule about where the choice comes from.

| `mode` | What |
|---|---|
| `color` | one flat colour |
| `image` | one PNG or JPEG, fitted five ways |
| `slideshow` | every image in a directory, in turn, with a crossfade |
| `scene` | `gradient`, `aurora`, `plasma` or `starfield` — computed, no file |

The rule is that the machine has a wallpaper and a user may override it:

1. **`~/.config/raven/canvas.toml`** — what you wrote. Beats everything.
2. **`/etc/raven/canvas.toml`** — what the image shipped. Ships with
   `[background]` commented out, deliberately; see below.
3. **`/usr/share/wallpaper/set/wallpaper.<ext>`** — the wallpaper this machine
   has set.
4. The built-in gradient, which needs nothing on disk.

Step 3 is the interesting one. That path is not this project's invention:
RavenLogin's greeter already draws the login screen on it when `login.toml`
names no wallpaper of its own, and `login.toml` describes it as *"the same file
huginn draws behind the session"*. Honouring it here is what makes that
sentence true — one picture, on the login screen and on the desktop, set in one
place:

```bash
sudo raven-set-wallpaper /path/to/image
```

That copies the image into `/usr/share/wallpaper` and points
`/usr/share/wallpaper/set/wallpaper.jpg` at it. The daemon watches `set/`, so
the desktop changes within a moment; the login screen picks it up on its next
start. Writing a `[background]` into `/etc/raven/canvas.toml` overrides all of
it for every user on the machine, which is why the shipped file does not have
one.

For yourself alone, ignore the machine's wallpaper entirely:

```bash
ravencanvas set image ~/pictures/cliff.jpg --persist
ravencanvas set slideshow ~/pictures --interval 300 --shuffle
ravencanvas set scene aurora --speed 0.5
ravencanvas status
```

Without `--persist` a change lasts until the daemon exits or the config file
changes underneath it.

## What it costs when it is not doing anything

Nothing, and that is a design goal rather than a hope.

A still wallpaper — an image, a colour, or a scene at `speed = 0` — is drawn
once, and then the process blocks on its four descriptors with **no timer
armed at all**. `Engine::next_wake` returns `None` for exactly those cases.

An animated one stops on its own when it cannot be seen. A compositor does not
send a frame callback for a surface it is not going to draw, and nothing here
is drawn until the previous callback arrives — so a wallpaper covered by a
full-screen window simply stops rendering, without being told and without a
line of code that knows about occlusion.

Scenes are drawn small and upscaled. A full-resolution procedural field at 4K
is genuinely too much arithmetic for one core; a smooth field of colour drawn
at 720 and upscaled is visually close to free, and the cost is quadratic in the
dial, so halving `render.detail` quarters the work.

## Why it is a separate process

huginn draws its own dock, launcher and quick settings inside its render loop,
because the design spec says the shell is not a client: anything that must feel
instant and must never fail does not get to be a separate process that can miss
a frame or die.

A wallpaper is precisely the case that rule is not about. It is *allowed* to
fail — the file may be missing, corrupt, or on a disk that is not mounted yet —
and huginn already paints its own background colour under everything, so the
worst thing this process's death can do is leave a plain desktop. RavenGUI's
`docs/protocols.md` says so directly: *"Panels, the dock and the wallpaper are
wlr-layer-shell surfaces. Do not duplicate those here."*

## Layout

```
crates/
├── raven-paint/           ★ pixels: decode, fit, composite. Pure.
├── raven-scene/           ★ the four procedural scenes. Pure.
├── raven-canvas-proto/      what the daemon and the CLI say to each other
├── ravencanvasd/            the daemon — Wayland, config, control socket
└── ravencanvas/             the CLI
```

★ marks a crate with no Wayland, no filesystem beyond `Image::load`, and no
clock. That is what makes the interesting questions testable without a
compositor: does `cover` crop or squash, does a crossfade at `t = 0` reproduce
its first input byte for byte, does a corrupt JPEG return an error or a panic.

## No unsafe

`[workspace.lints.rust] unsafe_code = "forbid"` in the root manifest, inherited
by every crate via `[lints] workspace = true`. `forbid` cannot be lifted by an
`#[allow]` anywhere inside a crate, so the only way to get unsafe into this
workspace is to drop the `[lints]` opt-in from a manifest — which is what
`scripts/check-unsafe.sh` exists to catch, and it runs in `imlazy check`.

Unlike RavenGUI and RavenLogin there is **no quarantine crate here, and there
should never be one**. This process decodes images off disk and writes into a
buffer the compositor also maps; both are exactly the places a memory-safety
bug turns into somebody else's problem. Everything it needs from the kernel,
`rustix` already exposes safely.

A wallpaper is the only attacker-shaped input this project takes — a user names
a path, or drops a file into a slideshow directory, and it decodes whatever is
there. Three things bound that, and they are the same three the greeter settled
on: the decoders are pure Rust, they are given explicit size limits *before*
they are given a file, and every failure is non-fatal. A wallpaper that will
not decode leaves the previous one on screen.

## Configuration

`config/canvas.toml` is the reference, installed to `/etc/raven/canvas.toml`
and never overwritten by a reinstall. Everything in it is a default the daemon
already has compiled in, so a machine without the file behaves identically.

A file that does not parse is a warning, not a fatal error — the opposite of
`ravend`, and deliberately. A wallpaper is not a policy, this daemon reloads on
every edit, and the moment you are most likely to have a broken file is halfway
through editing one. A bad file leaves what is already on screen.

Both config files are watched, and so is a slideshow directory, and so is
`/usr/share/wallpaper/set` while nothing is overriding it. Directories are
watched rather than files, because editors replace files rather than modifying
them and a watch on the original follows an inode that has just been unlinked.

## Control socket

`$XDG_RUNTIME_DIR/raven-canvas/control.sock`, protected by the directory it
lives in and nothing else — the kernel and the session manager have already
made that `0700` and owned by you.

A 4-byte big-endian length then that many bytes of JSON, which is the same
framing RavenLogin's greeter protocol uses: the message rate is a few per
session, and being able to read the traffic with `socat` while bringing the
thing up is worth more than the bytes.

Values cross it *named* rather than resolved — a colour is `"#7AA2F7"` and a
scene is `"aurora"` — so that a value from the config file and a value from the
CLI go through exactly one parser in the daemon and cannot disagree about what
`#7AF` means.

## Development

```bash
imlazy run          # against the session you are already in
imlazy preview      # scene=plasma imlazy preview
imlazy check        # fmt, clippy -D warnings, the unsafe check, tests
imlazy test
```

`--preview` renders one frame to a PNG with no compositor, no socket and no
config, which is the fastest way to look at a scene:

```bash
ravencanvasd --preview /tmp/a.png 1920x1080 12 starfield
```

`RAVEN_CANVAS_SOCKET` overrides the socket path, so a second daemon can be run
against a nested compositor without fighting the real one for it.

## Licence

MIT OR Apache-2.0.
