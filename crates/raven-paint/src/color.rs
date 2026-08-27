//! A colour, and the one string format the config accepts for one.

use std::fmt;
use std::str::FromStr;

/// A colour, as `0xAARRGGBB`.
///
/// Packed into one integer so it is `Copy` and comparable, and converted at
/// the edges: `wl_shm`'s `Argb8888` wants this exact layout, little-endian.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color(u32);

impl Color {
    /// Fully transparent. Only ever a starting value; nothing is handed to the
    /// compositor with alpha in it.
    pub const TRANSPARENT: Self = Self(0);

    /// Black, opaque. The colour of a screen with nothing configured on it.
    pub const BLACK: Self = Self(0xFF00_0000);

    #[must_use]
    pub const fn from_argb(argb: u32) -> Self {
        Self(argb)
    }

    /// From three channels, opaque.
    #[must_use]
    pub const fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(0xFF00_0000 | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32)
    }

    #[must_use]
    pub const fn argb(self) -> u32 {
        self.0
    }

    #[must_use]
    pub const fn alpha(self) -> u8 {
        (self.0 >> 24) as u8
    }

    #[must_use]
    pub const fn red(self) -> u8 {
        (self.0 >> 16) as u8
    }

    #[must_use]
    pub const fn green(self) -> u8 {
        (self.0 >> 8) as u8
    }

    #[must_use]
    pub const fn blue(self) -> u8 {
        self.0 as u8
    }

    /// The same colour at a different opacity.
    #[must_use]
    pub const fn with_alpha(self, alpha: u8) -> Self {
        Self((self.0 & 0x00FF_FFFF) | ((alpha as u32) << 24))
    }

    /// This colour's four bytes in the layout every buffer here uses.
    #[must_use]
    pub const fn to_bgra(self) -> [u8; 4] {
        [self.blue(), self.green(), self.red(), self.alpha()]
    }

    /// Mix towards `other`, `t` in `0.0..=1.0`.
    ///
    /// Channel-wise and in gamma space, not linear light. That is the wrong
    /// answer for compositing photographs and the right one here: every use of
    /// this is a ramp between two colours somebody chose by looking at them,
    /// and interpolating those in linear light makes the midpoint visibly
    /// lighter than the colour picker they were chosen in.
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |a: u8, b: u8| -> u8 {
            (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
        };
        Self::from_argb(
            ((mix(self.alpha(), other.alpha()) as u32) << 24)
                | ((mix(self.red(), other.red()) as u32) << 16)
                | ((mix(self.green(), other.green()) as u32) << 8)
                | mix(self.blue(), other.blue()) as u32,
        )
    }

    /// Perceived brightness, `0.0..=1.0`.
    ///
    /// Rec. 601 luma. Used only to decide whether a generated palette needs a
    /// darker or a lighter partner, which is a judgement coarse enough that
    /// the difference between this and Rec. 709 does not reach the screen.
    #[must_use]
    pub fn luma(self) -> f32 {
        (0.299 * f32::from(self.red())
            + 0.587 * f32::from(self.green())
            + 0.114 * f32::from(self.blue()))
            / 255.0
    }
}

/// Hex, the way it is written in the config file.
///
/// Deliberately narrow: `#RGB`, `#RRGGBB` and `#AARRGGBB`, with the `#`
/// optional. Named colours are not accepted, because a wallpaper config that
/// understands `rebeccapurple` is a config that has to keep understanding it
/// forever.
impl FromStr for Color {
    type Err = ParseColorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ParseColorError(s.to_string()));
        }

        let value = u32::from_str_radix(hex, 16).map_err(|_| ParseColorError(s.to_string()))?;
        match hex.len() {
            // #RGB, each digit doubled: #7AF is #77AAFF.
            3 => {
                let expand = |nibble: u32| (nibble << 4) | nibble;
                Ok(Self::rgb(
                    expand((value >> 8) & 0xF) as u8,
                    expand((value >> 4) & 0xF) as u8,
                    expand(value & 0xF) as u8,
                ))
            }
            6 => Ok(Self::from_argb(0xFF00_0000 | value)),
            8 => Ok(Self::from_argb(value)),
            _ => Err(ParseColorError(s.to_string())),
        }
    }
}

/// Written back the way it would be read, so a config this daemon rewrites is
/// a config the user still recognises.
impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.alpha() == 0xFF {
            write!(f, "#{:06X}", self.0 & 0x00FF_FFFF)
        } else {
            write!(f, "#{:08X}", self.0)
        }
    }
}

/// `Debug` is the same text as `Display`. The derived one prints a decimal
/// integer, which is unreadable in a log line about a colour.
impl fmt::Debug for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A string that is not a colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseColorError(String);

impl fmt::Display for ParseColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a colour; write #RGB, #RRGGBB or #AARRGGBB",
            self.0
        )
    }
}

impl std::error::Error for ParseColorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_digits_are_opaque() {
        let c: Color = "#7AA2F7".parse().unwrap();
        assert_eq!(
            (c.red(), c.green(), c.blue(), c.alpha()),
            (0x7A, 0xA2, 0xF7, 0xFF)
        );
    }

    #[test]
    fn eight_digits_carry_their_own_alpha() {
        let c: Color = "#807AA2F7".parse().unwrap();
        assert_eq!(c.alpha(), 0x80);
        assert_eq!(c.red(), 0x7A);
    }

    #[test]
    fn three_digits_double_each_nibble() {
        assert_eq!(
            "#7AF".parse::<Color>().unwrap(),
            Color::rgb(0x77, 0xAA, 0xFF)
        );
    }

    #[test]
    fn the_hash_is_optional() {
        assert_eq!(
            "16161F".parse::<Color>().unwrap(),
            "#16161F".parse().unwrap()
        );
    }

    #[test]
    fn nonsense_is_an_error_rather_than_a_colour() {
        for bad in [
            "",
            "#",
            "#12",
            "#1234567",
            "rebeccapurple",
            "#GGGGGG",
            "#-1",
        ] {
            assert!(bad.parse::<Color>().is_err(), "{bad:?} should not parse");
        }
    }

    /// A colour written by this must be readable by it. The config file is
    /// rewritten by `ravencanvas set`, so this round trip is a user's file
    /// surviving a change made through the CLI.
    #[test]
    fn display_round_trips_through_parsing() {
        for argb in [0xFF16_161F, 0x807A_A2F7, 0x0000_0000, 0xFFFF_FFFF] {
            let c = Color::from_argb(argb);
            assert_eq!(c.to_string().parse::<Color>().unwrap(), c, "{c}");
        }
    }

    #[test]
    fn the_byte_order_is_blue_first() {
        assert_eq!(
            Color::from_argb(0xFF11_2233).to_bgra(),
            [0x33, 0x22, 0x11, 0xFF]
        );
    }

    #[test]
    fn lerp_hits_both_ends_exactly() {
        let a = Color::rgb(0, 0, 0);
        let b = Color::rgb(255, 255, 255);
        assert_eq!(a.lerp(b, 0.0), a);
        assert_eq!(a.lerp(b, 1.0), b);
        // And is clamped, so an overshooting animation cannot wrap a channel.
        assert_eq!(a.lerp(b, -5.0), a);
        assert_eq!(a.lerp(b, 5.0), b);
    }

    #[test]
    fn lerp_lands_in_the_middle() {
        let mid = Color::rgb(0, 0, 0).lerp(Color::rgb(200, 100, 50), 0.5);
        assert_eq!((mid.red(), mid.green(), mid.blue()), (100, 50, 25));
    }

    #[test]
    fn luma_orders_black_below_white() {
        assert!(Color::BLACK.luma() < 0.01);
        assert!(Color::rgb(255, 255, 255).luma() > 0.99);
        assert!(Color::rgb(0x16, 0x16, 0x1F).luma() < 0.2);
    }
}
