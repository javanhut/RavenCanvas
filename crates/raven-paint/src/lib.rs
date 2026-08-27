//! Pixels: decoding an image, fitting it to a screen, and compositing.
//!
//! Everything in this crate is a pure function of its inputs. There is no
//! Wayland here, no filesystem beyond [`Image::load`], and no clock. That is
//! deliberate and it is what makes the wallpaper engine testable at all: the
//! interesting questions -- does `cover` crop or squash, does a crossfade at
//! `t = 0` produce exactly the first image, does a corrupt JPEG return an
//! error or a panic -- are all answerable without a compositor, a screen or a
//! login session.
//!
//! # The pixel layout, once
//!
//! Every buffer in this crate is `0xAARRGGBB` written little-endian, so the
//! bytes of a pixel are `[B, G, R, A]`. That is what `wl_shm`'s `Argb8888`
//! means, and converting into it happens exactly once -- in [`image`], as the
//! decoder's output is copied out. Nothing downstream ever has to think about
//! channel order again, and in particular the per-frame paths never swap
//! anything.
//!
//! # What is trusted here
//!
//! A wallpaper is the only attacker-shaped input this project takes: a user
//! names a path, or drops a file into a slideshow directory, and [`image`]
//! decodes whatever is at it. Three things bound that, and they are the same
//! three RavenLogin's greeter settled on:
//!
//! - The decoders are pure-Rust and this crate forbids `unsafe`, so a
//!   malformed image is a `Result::Err` rather than a corrupted stack.
//! - Both decoders are given explicit limits before they are given a file, so
//!   a 40-byte header claiming 60000x60000 is refused rather than allocated.
//! - Every failure is non-fatal to the caller. A wallpaper that will not
//!   decode leaves the previous one on screen; see `ravencanvasd`'s `engine`.

#![forbid(unsafe_code)]

mod canvas;
mod color;
mod dither;
mod field;
mod image;

pub use canvas::Canvas;
pub use color::{Color, ParseColorError};
pub use field::Field;
pub use image::{Fit, Image, MAX_FILE_BYTES, MAX_PIXELS, ParseFitError};

/// The bytes one pixel occupies, everywhere in this crate.
pub const BYTES_PER_PIXEL: usize = 4;

/// The size in bytes of a `width` x `height` buffer in this crate's layout.
///
/// Returns `None` rather than wrapping, because the one caller that matters
/// is allocating an shm pool from numbers a compositor sent it.
#[must_use]
pub fn buffer_len(width: i32, height: i32) -> Option<usize> {
    let w = usize::try_from(width).ok()?;
    let h = usize::try_from(height).ok()?;
    w.checked_mul(h)?.checked_mul(BYTES_PER_PIXEL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_length_is_four_bytes_a_pixel() {
        assert_eq!(buffer_len(1920, 1080), Some(1920 * 1080 * 4));
        assert_eq!(buffer_len(0, 0), Some(0));
    }

    #[test]
    fn a_negative_dimension_has_no_length() {
        assert_eq!(buffer_len(-1, 10), None);
        assert_eq!(buffer_len(10, -1), None);
    }

    /// The case this function exists for: a compositor, or a bug in this
    /// process, produces dimensions whose product is enormous. What must never
    /// come back is a *small* number -- a wrapping multiply would give one,
    /// and it would then be allocated and drawn a screen's worth of pixels
    /// past.
    #[test]
    fn an_enormous_size_never_wraps_to_a_small_one() {
        assert!(
            buffer_len(i32::MAX, i32::MAX).is_none_or(|n| n > 1 << 40),
            "the length wrapped"
        );
    }
}
