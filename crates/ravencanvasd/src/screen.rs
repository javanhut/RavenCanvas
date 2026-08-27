//! One output's layer surface, and the pixels on it.
//!
//! # Why the wallpaper is a client
//!
//! huginn draws its own dock, launcher and quick settings inside its render
//! loop, because the design spec says the shell is not a client: anything that
//! must feel instant and must never fail does not get to be a separate process
//! that can miss a frame or die.
//!
//! A wallpaper is the case that rule is *not* about. It may fail -- it is a
//! picture, and the picture may be missing, corrupt, or on a disk that is not
//! mounted yet -- and huginn already paints its own background colour under
//! everything, so the worst outcome of this process dying is a plain desktop
//! rather than a broken one. `docs/protocols.md` in RavenGUI says as much:
//! *"Panels, the dock and the wallpaper are wlr-layer-shell surfaces."*
//!
//! # How the surface is set up
//!
//! Anchored to all four edges at size 0x0, which is layer-shell for "fill the
//! output": the compositor answers with the output's real dimensions. Then
//! three requests that all say the same thing in different ways -- this is
//! scenery, not furniture:
//!
//! - `Layer::Background`, so it is behind every window and every panel.
//! - `set_exclusive_zone(-1)`, so it neither reserves space nor is pushed
//!   around by anything that does. A wallpaper covers the whole output
//!   including the strip under a panel.
//! - `KeyboardInteractivity::None` and an **empty input region**. The first is
//!   the protocol default and huginn would demote a stronger request anyway;
//!   the second is the one that actually matters, because without it every
//!   click on the desktop lands on this surface, which does nothing with it.
//!   An empty input region makes the pointer pass through to whatever huginn
//!   would otherwise have hit.
//!
//! The buffer format is `Xrgb8888` rather than `Argb8888`, and the whole
//! surface is declared opaque. Both are true -- everything here writes a
//! filled ground -- and both let the compositor skip blending this surface and
//! skip drawing it at all where a window covers it.

use raven_paint::{Canvas, Color, Field, Fit, Image};
use smithay_client_toolkit::compositor::{CompositorState, FrameCallbackData, Region};
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface,
};
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::slot::SlotPool;

use crate::app::App;
use crate::engine::Frame;

/// The layer-shell namespace this surface identifies itself by.
///
/// Visible to anything inspecting the compositor's surfaces, so it is the
/// binary's name rather than something generic.
pub(crate) const NAMESPACE: &str = "ravencanvas";

/// The pool each screen starts with, in bytes.
///
/// One 1080p frame. It grows when a configure says the screen is larger, and
/// again when a second buffer is needed because the compositor still holds the
/// first. Starting here rather than at the largest imaginable size keeps a
/// daemon on a small panel from mapping thirty megabytes it will never touch.
const INITIAL_POOL: usize = 1920 * 1080 * 4;

/// One screen.
pub(crate) struct Screen {
    output: wl_output::WlOutput,
    /// The connector name, when the compositor gave one.
    name: String,
    layer: LayerSurface,
    pool: SlotPool,

    width: i32,
    height: i32,
    scale: i32,
    configured: bool,

    /// Whether a frame callback is outstanding.
    ///
    /// This is the daemon's back-pressure, and it is doing more work than it
    /// looks like. A compositor does not send a frame callback for a surface
    /// it is not going to draw, so a wallpaper completely covered by a
    /// full-screen window simply stops being asked for frames -- and because
    /// nothing is drawn until the previous callback arrives, this process
    /// stops rendering too, without ever being told why. That is the whole of
    /// the occlusion handling, and it is free.
    awaiting_frame: bool,

    /// Scratch for scene rendering. Owned per screen because two screens of
    /// different sizes need different field sizes.
    field: Field,
    cache: FitCache,
}

/// Hand-written: the derived one would print a `SlotPool` and a `Field`.
impl std::fmt::Debug for Screen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Screen")
            .field("name", &self.name)
            .field("size", &(self.width, self.height))
            .field("scale", &self.scale)
            .field("configured", &self.configured)
            .field("awaiting_frame", &self.awaiting_frame)
            .finish_non_exhaustive()
    }
}

impl Screen {
    /// Create a layer surface on `output`.
    ///
    /// Concrete in [`App`](crate::app::App) rather than generic over the
    /// dispatch state. There is exactly one state type in this daemon, and
    /// spelling out the four `Dispatch` bounds that would make this generic
    /// buys nothing but a paragraph of `where` clause.
    pub(crate) fn new(
        compositor: &CompositorState,
        layer_shell: &LayerShell,
        shm: &Shm,
        qh: &QueueHandle<App>,
        output: wl_output::WlOutput,
        name: String,
    ) -> anyhow::Result<Self> {
        let surface = compositor.create_surface(qh);
        let layer = layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some(NAMESPACE),
            Some(&output),
        );

        // All four edges at 0x0: fill the output, whatever size it is.
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        // -1, not 0. Zero means "reserve nothing but respect what others
        // reserved", which would shrink the wallpaper to fit around a panel
        // and leave a hole where the panel is translucent.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        // An empty region, not `None`: `None` means "the whole surface", which
        // is the default and the opposite of what a wallpaper wants. This is
        // what lets clicks fall through to the desktop.
        match Region::new(compositor) {
            Ok(empty) => layer.set_input_region(Some(empty.wl_region())),
            Err(e) => tracing::warn!(
                "cannot create an empty input region; clicks on the desktop will land on the wallpaper: {e}"
            ),
        }

        layer.commit();

        let pool = SlotPool::new(INITIAL_POOL, shm)
            .map_err(|e| anyhow::anyhow!("cannot create an shm pool: {e}"))?;

        Ok(Self {
            output,
            name,
            layer,
            pool,
            width: 0,
            height: 0,
            scale: 1,
            configured: false,
            awaiting_frame: false,
            field: Field::new(1, 1),
            cache: FitCache::default(),
        })
    }

    pub(crate) fn output(&self) -> &wl_output::WlOutput {
        &self.output
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }

    pub(crate) fn scale(&self) -> i32 {
        self.scale
    }

    pub(crate) fn owns(&self, surface: &wl_surface::WlSurface) -> bool {
        self.layer.wl_surface() == surface
    }

    pub(crate) fn matches_layer(&self, layer: &LayerSurface) -> bool {
        self.layer.wl_surface() == layer.wl_surface()
    }

    /// Whether a redraw is worth attempting: the surface exists, has a size,
    /// and the compositor is ready for another frame.
    pub(crate) fn is_ready(&self) -> bool {
        self.configured && self.width > 0 && self.height > 0 && !self.awaiting_frame
    }

    /// The compositor answered our configure. Returns whether the size
    /// changed, which is what tells the caller a redraw is unavoidable.
    pub(crate) fn configured(&mut self, width: u32, height: u32) -> bool {
        let (width, height) = (width as i32, height as i32);
        let changed = !self.configured || width != self.width || height != self.height;

        self.width = width;
        self.height = height;
        self.configured = true;
        // A configure is an acknowledgement that the compositor wants a new
        // buffer, so any callback we were waiting on is moot.
        self.awaiting_frame = false;
        if changed {
            // Every scaled copy was for the old size.
            self.cache.clear();
        }
        changed
    }

    /// A frame callback arrived; the compositor is ready for another.
    pub(crate) fn frame_done(&mut self) {
        self.awaiting_frame = false;
    }

    pub(crate) fn set_scale(&mut self, scale: i32) -> bool {
        let scale = scale.max(1);
        if scale == self.scale {
            return false;
        }
        self.scale = scale;
        // Told to the compositor so it knows the buffer is already at this
        // density and does not resample it. The buffer size does not change:
        // layer-shell configures in surface-local coordinates and huginn
        // advertises an integer scale, so `width * scale` is what a
        // hidpi-aware client would allocate -- but huginn also lays the
        // desktop out at a fractional scale and resamples, so following its
        // advertised integer scale is what `docs/integration.md` asks for.
        let _ = self.layer.set_buffer_scale(scale.max(1) as u32);
        true
    }

    /// Draw one frame and commit it.
    ///
    /// Returns whether anything was actually put on screen. `false` means the
    /// buffer could not be allocated, which is worth knowing about but is not
    /// worth failing over -- the previous frame stays up and the next attempt
    /// may succeed.
    pub(crate) fn draw(&mut self, frame: &Frame<'_>, detail: u32, qh: &QueueHandle<App>) -> bool {
        if !self.configured || self.width <= 0 || self.height <= 0 {
            return false;
        }

        let (width, height) = (self.width, self.height);
        let stride = width * 4;

        // Xrgb8888: this surface is opaque, and saying so lets the compositor
        // skip blending it and skip drawing it where something covers it.
        let buffer = self
            .pool
            .create_buffer(width, height, stride, wl_shm::Format::Xrgb8888);
        let (buffer, bytes) = match buffer {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(
                    output = %self.name,
                    "cannot allocate a {width}x{height} buffer: {e}"
                );
                return false;
            }
        };

        paint(
            &mut Canvas::new(bytes, width, height),
            frame,
            &mut self.field,
            &mut self.cache,
            detail,
        );

        let surface = self.layer.wl_surface();
        // The whole surface, every time. A wallpaper's damage is either
        // nothing -- in which case this frame would not have been drawn -- or
        // everything, because a scene changes every pixel and an image change
        // replaces the picture. Tracking finer damage would cost more than it
        // saved.
        surface.damage_buffer(0, 0, width, height);

        // Asked for *before* the commit that carries the new buffer, so the
        // callback belongs to this frame.
        surface.frame(qh, FrameCallbackData(surface.clone()));

        if let Err(e) = buffer.attach_to(surface) {
            tracing::warn!(output = %self.name, "cannot attach a buffer: {e}");
            return false;
        }
        self.layer.commit();
        self.awaiting_frame = true;
        true
    }

    /// Declare the whole surface opaque.
    ///
    /// Separate from [`Screen::new`] because the size is not known until the
    /// first configure, and an opaque region has to name a rectangle.
    pub(crate) fn set_opaque(&self, compositor: &CompositorState) {
        if self.width <= 0 || self.height <= 0 {
            return;
        }
        match Region::new(compositor) {
            Ok(region) => {
                region.add(0, 0, self.width, self.height);
                self.layer.set_opaque_region(Some(region.wl_region()));
            }
            Err(e) => tracing::debug!("cannot declare the wallpaper opaque: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

/// Put a [`Frame`] onto a canvas.
///
/// Free rather than a method on [`Screen`], so that `--preview` renders
/// through *this exact code* rather than through a second implementation that
/// would drift. A preview whose output is not what the compositor would show
/// is worse than no preview.
///
/// `cache` and `field` are the caller's scratch. Both are reused frame to
/// frame; see [`FitCache`] for why the cache is trimmed here rather than left
/// to grow.
pub(crate) fn paint(
    canvas: &mut Canvas<'_>,
    frame: &Frame<'_>,
    field: &mut Field,
    cache: &mut FitCache,
    detail: u32,
) {
    let (width, height) = (canvas.width(), canvas.height());
    if width <= 0 || height <= 0 {
        return;
    }

    match frame {
        Frame::Color(colour) => {
            cache.clear();
            canvas.fill(*colour);
        }
        Frame::Scene { scene, time } => {
            cache.clear();
            let edge = if detail == 0 {
                scene.field_edge()
            } else {
                detail
            };
            scene.render_at(canvas, field, *time, edge);
        }
        Frame::Image {
            id,
            image,
            fit,
            background,
        } => {
            let key = FitKey::new(*id, width, height, *fit, *background);
            cache.ensure(key, image);
            cache.retain(1);
            match cache.get(key) {
                Some(pixels) => canvas.blit(pixels),
                // Cannot happen -- `ensure` ran on the line above -- but a
                // wallpaper daemon should not panic to prove it.
                None => canvas.fill(*background),
            }
        }
        Frame::Crossfade {
            from,
            to,
            fit,
            background,
            progress,
        } => {
            let (a, b) = (
                FitKey::new(from.0, width, height, *fit, *background),
                FitKey::new(to.0, width, height, *fit, *background),
            );
            cache.ensure(a, from.1);
            cache.ensure(b, to.1);
            cache.retain(2);

            match (cache.get(a), cache.get(b)) {
                (Some(a), Some(b)) => canvas.crossfade(a, b, *progress),
                // One of the two would not scale. Showing the destination
                // whole is better than showing nothing, and the fade is about
                // to end at it anyway.
                (_, Some(one)) | (Some(one), None) => canvas.blit(one),
                (None, None) => canvas.fill(*background),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The scaled-image cache
// ---------------------------------------------------------------------------

/// Everything that decides what a scaled copy looks like.
///
/// The image's identity is a number rather than its path, because an image
/// reloaded from the same path is a different image -- somebody may have
/// replaced the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FitKey {
    id: u64,
    width: i32,
    height: i32,
    fit: Fit,
    background: Color,
}

impl FitKey {
    const fn new(id: u64, width: i32, height: i32, fit: Fit, background: Color) -> Self {
        Self {
            id,
            width,
            height,
            fit,
            background,
        }
    }
}

/// The last few scaled copies, newest last.
///
/// Two entries is the working set: one image on screen, and during a crossfade
/// the one it is fading from. Each is a full screen of pixels -- 33 MB at 4K --
/// which is why [`FitCache::retain`] is called on every frame rather than left
/// to a size limit. A fade that has finished must give its memory back
/// immediately, not eventually.
#[derive(Debug, Default)]
pub(crate) struct FitCache {
    entries: Vec<(FitKey, Vec<u8>)>,
}

impl FitCache {
    /// Scale `image` for `key` if it is not already scaled.
    fn ensure(&mut self, key: FitKey, image: &Image) {
        if self.entries.iter().any(|(existing, _)| *existing == key) {
            return;
        }
        tracing::debug!(
            width = key.width,
            height = key.height,
            fit = %key.fit,
            "scaling a wallpaper"
        );
        let pixels = image.fit_into(key.width, key.height, key.fit, key.background);
        self.entries.push((key, pixels));
    }

    fn get(&self, key: FitKey) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|(existing, _)| *existing == key)
            .map(|(_, pixels)| pixels.as_slice())
    }

    /// Keep the `count` most recently added entries.
    fn retain(&mut self, count: usize) {
        while self.entries.len() > count {
            self.entries.remove(0);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Whether an output should be drawn on at all, and what to call it.
///
/// Split out from [`Screen`] so it can be tested without a compositor: the
/// naming rule -- prefer the connector name, fall back to something stable and
/// unique -- is the kind of thing that is otherwise only discovered by
/// plugging in a monitor.
pub(crate) fn output_name(connector: Option<&str>, description: Option<&str>, id: u32) -> String {
    match connector.filter(|name| !name.is_empty()) {
        Some(name) => name.to_string(),
        None => match description.filter(|text| !text.is_empty()) {
            Some(text) => text.to_string(),
            None => format!("output-{id}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(level: u8) -> Image {
        Image::from_bgra(2, 2, vec![level; 16]).expect("image")
    }

    fn key(id: u64, width: i32) -> FitKey {
        FitKey::new(id, width, 4, Fit::Cover, Color::BLACK)
    }

    #[test]
    fn a_cache_miss_scales_and_a_hit_does_not() {
        let mut cache = FitCache::default();
        cache.ensure(key(1, 8), &image(0x10));
        assert_eq!(cache.entries.len(), 1);

        cache.ensure(key(1, 8), &image(0x99));
        assert_eq!(cache.entries.len(), 1, "the same key scaled twice");
        // The second image was never looked at, which is the point: the key
        // says it is the same picture at the same size.
        assert_eq!(cache.get(key(1, 8)).unwrap()[0], 0x10);
    }

    #[test]
    fn a_different_size_is_a_different_entry() {
        let mut cache = FitCache::default();
        cache.ensure(key(1, 8), &image(0x10));
        cache.ensure(key(1, 16), &image(0x10));
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.get(key(1, 8)).unwrap().len(), 8 * 4 * 4);
        assert_eq!(cache.get(key(1, 16)).unwrap().len(), 16 * 4 * 4);
    }

    #[test]
    fn a_different_fit_or_background_is_a_different_entry() {
        let mut cache = FitCache::default();
        cache.ensure(FitKey::new(1, 8, 4, Fit::Cover, Color::BLACK), &image(0x10));
        cache.ensure(
            FitKey::new(1, 8, 4, Fit::Contain, Color::BLACK),
            &image(0x10),
        );
        cache.ensure(
            FitKey::new(1, 8, 4, Fit::Cover, Color::rgb(1, 2, 3)),
            &image(0x10),
        );
        assert_eq!(cache.entries.len(), 3);
    }

    /// The memory rule: a finished crossfade gives its second buffer back on
    /// the very next frame, not whenever a size limit happens to bite.
    #[test]
    fn retaining_drops_the_oldest_entries_first() {
        let mut cache = FitCache::default();
        cache.ensure(key(1, 8), &image(0x10));
        cache.ensure(key(2, 8), &image(0x20));
        cache.retain(1);

        assert_eq!(cache.entries.len(), 1);
        assert!(cache.get(key(1, 8)).is_none(), "the older entry survived");
        assert!(
            cache.get(key(2, 8)).is_some(),
            "the newer entry was dropped"
        );
    }

    #[test]
    fn retaining_more_than_there_is_keeps_everything() {
        let mut cache = FitCache::default();
        cache.ensure(key(1, 8), &image(0x10));
        cache.retain(5);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn clearing_frees_everything() {
        let mut cache = FitCache::default();
        cache.ensure(key(1, 8), &image(0x10));
        cache.clear();
        assert!(cache.get(key(1, 8)).is_none());
    }

    // -- naming -------------------------------------------------------------

    #[test]
    fn an_output_is_named_by_its_connector() {
        assert_eq!(
            output_name(Some("eDP-1"), Some("a laptop panel"), 7),
            "eDP-1"
        );
    }

    #[test]
    fn without_a_connector_the_description_will_do() {
        assert_eq!(output_name(None, Some("Dell U2720Q"), 7), "Dell U2720Q");
        assert_eq!(output_name(Some(""), Some("Dell U2720Q"), 7), "Dell U2720Q");
    }

    /// Something unique is always produced. The name goes in a status reply
    /// and in log lines, and two nameless outputs must be tellable apart.
    #[test]
    fn with_neither_the_id_is_used() {
        assert_eq!(output_name(None, None, 7), "output-7");
        assert_eq!(output_name(Some(""), Some(""), 7), "output-7");
        assert_ne!(output_name(None, None, 7), output_name(None, None, 8));
    }
}
