//! RGBA colors parsed from and serialized to CSS-style hex strings.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Why a hex color string could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ColorParseError {
    /// The input was empty (or only whitespace).
    #[error("color is empty; expected a hex color such as \"#1e1f22\"")]
    Empty,
    /// The input did not start with `#`.
    #[error("color {input:?} must start with '#' (e.g. \"#1e1f22\")")]
    MissingHash {
        /// The rejected input.
        input: String,
    },
    /// The number of hex digits was not 3, 4, 6 or 8.
    #[error(
        "color {input:?} has {digits} hex digit(s); expected 3 (#rgb), 4 (#rgba), 6 (#rrggbb) or 8 (#rrggbbaa)"
    )]
    InvalidLength {
        /// The rejected input.
        input: String,
        /// Number of characters after `#`.
        digits: usize,
    },
    /// A character after `#` was not a hex digit.
    #[error("color {input:?} contains invalid hex digit {ch:?} at position {position}")]
    InvalidDigit {
        /// The rejected input.
        input: String,
        /// The offending character.
        ch: char,
        /// Character index of `ch` within `input` (the `#` is position 0).
        position: usize,
    },
}

/// An 8-bit-per-channel sRGB color with straight (non-premultiplied) alpha.
///
/// Parses from `#rgb`, `#rgba`, `#rrggbb` and `#rrggbbaa` (case-insensitive) and
/// serializes back to lowercase `#rrggbb`, or `#rrggbbaa` when not fully opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel; 255 is fully opaque.
    pub a: u8,
}

impl Rgba {
    /// Opaque black.
    pub const BLACK: Rgba = Rgba::rgb(0, 0, 0);
    /// Opaque white.
    pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);
    /// Fully transparent black.
    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);

    /// Creates a color from all four channels.
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Creates a fully opaque color.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 255)
    }

    /// Parses a hex color string. Surrounding whitespace is ignored.
    pub fn from_hex(input: &str) -> Result<Self, ColorParseError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(ColorParseError::Empty);
        }
        let Some(digits) = trimmed.strip_prefix('#') else {
            return Err(ColorParseError::MissingHash {
                input: trimmed.to_owned(),
            });
        };

        let mut nibbles: Vec<u8> = Vec::with_capacity(8);
        for (i, ch) in digits.chars().enumerate() {
            // to_digit(16) yields 0..=15, which always fits in a u8.
            match ch.to_digit(16).and_then(|v| u8::try_from(v).ok()) {
                Some(v) => nibbles.push(v),
                None => {
                    return Err(ColorParseError::InvalidDigit {
                        input: trimmed.to_owned(),
                        ch,
                        position: i + 1,
                    })
                }
            }
        }

        let short = |n: u8| (n << 4) | n;
        let long = |hi: u8, lo: u8| (hi << 4) | lo;
        match *nibbles.as_slice() {
            [r, g, b] => Ok(Self::rgb(short(r), short(g), short(b))),
            [r, g, b, a] => Ok(Self::new(short(r), short(g), short(b), short(a))),
            [r1, r2, g1, g2, b1, b2] => Ok(Self::rgb(long(r1, r2), long(g1, g2), long(b1, b2))),
            [r1, r2, g1, g2, b1, b2, a1, a2] => Ok(Self::new(
                long(r1, r2),
                long(g1, g2),
                long(b1, b2),
                long(a1, a2),
            )),
            _ => Err(ColorParseError::InvalidLength {
                input: trimmed.to_owned(),
                digits: nibbles.len(),
            }),
        }
    }

    /// Formats as lowercase `#rrggbb`, or `#rrggbbaa` when alpha is not 255.
    pub fn to_hex(self) -> String {
        if self.a == 255 {
            format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.r, self.g, self.b, self.a)
        }
    }

    /// Returns `true` when alpha is 255.
    pub const fn is_opaque(self) -> bool {
        self.a == 255
    }

    /// Returns the same color with a different alpha.
    pub const fn with_alpha(self, a: u8) -> Self {
        Self::new(self.r, self.g, self.b, a)
    }

    /// Composites `self` over `background` ("source over") and returns an opaque color.
    ///
    /// The background's own alpha is ignored; it is treated as opaque.
    pub fn over(self, background: Rgba) -> Rgba {
        let a = u32::from(self.a);
        let mix = |fg: u8, bg: u8| -> u8 {
            let v = (u32::from(fg) * a + u32::from(bg) * (255 - a) + 127) / 255;
            // A weighted average of two u8 values is always <= 255.
            u8::try_from(v).unwrap_or(u8::MAX)
        };
        Rgba::rgb(
            mix(self.r, background.r),
            mix(self.g, background.g),
            mix(self.b, background.b),
        )
    }

    /// WCAG 2.x relative luminance in `0.0..=1.0`. Alpha is ignored.
    pub fn relative_luminance(self) -> f64 {
        fn linear(channel: u8) -> f64 {
            let c = f64::from(channel) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// WCAG 2.x contrast ratio between two colors, in `1.0..=21.0`.
    ///
    /// A translucent `self` is first composited over `other`, which is treated as
    /// opaque. For opaque colors the argument order does not matter.
    pub fn contrast_ratio(self, other: Rgba) -> f64 {
        let l1 = self.over(other).relative_luminance();
        let l2 = other.relative_luminance();
        let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Channels as floats in `0.0..=1.0`, in `[r, g, b, a]` order (handy for UI toolkits).
    pub fn to_f32_array(self) -> [f32; 4] {
        [self.r, self.g, self.b, self.a].map(|c| f32::from(c) / 255.0)
    }
}

impl fmt::Display for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for Rgba {
    type Err = ColorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for Rgba {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct HexVisitor;

        impl Visitor<'_> for HexVisitor {
            type Value = Rgba;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a hex color string such as \"#1e1f22\"")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Rgba, E> {
                Rgba::from_hex(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(HexVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_supported_forms() {
        assert_eq!(Rgba::from_hex("#abc").unwrap(), Rgba::rgb(0xaa, 0xbb, 0xcc));
        assert_eq!(
            Rgba::from_hex("#abcd").unwrap(),
            Rgba::new(0xaa, 0xbb, 0xcc, 0xdd)
        );
        assert_eq!(
            Rgba::from_hex("#1E1f22").unwrap(),
            Rgba::rgb(0x1e, 0x1f, 0x22)
        );
        assert_eq!(
            Rgba::from_hex("  #11223380 ").unwrap(),
            Rgba::new(0x11, 0x22, 0x33, 0x80)
        );
        assert_eq!("#fff".parse::<Rgba>().unwrap(), Rgba::WHITE);
    }

    #[test]
    fn reports_precise_errors() {
        assert_eq!(Rgba::from_hex("  "), Err(ColorParseError::Empty));
        assert!(matches!(
            Rgba::from_hex("1e1f22"),
            Err(ColorParseError::MissingHash { .. })
        ));
        assert_eq!(
            Rgba::from_hex("#12345"),
            Err(ColorParseError::InvalidLength {
                input: "#12345".into(),
                digits: 5
            })
        );
        assert_eq!(
            Rgba::from_hex("#"),
            Err(ColorParseError::InvalidLength {
                input: "#".into(),
                digits: 0
            })
        );
        assert_eq!(
            Rgba::from_hex("#12g456"),
            Err(ColorParseError::InvalidDigit {
                input: "#12g456".into(),
                ch: 'g',
                position: 3
            })
        );
        // Non-ASCII input must be reported, not panic on byte slicing.
        assert!(matches!(
            Rgba::from_hex("#\u{e9}\u{e9}\u{e9}"),
            Err(ColorParseError::InvalidDigit { ch: '\u{e9}', .. })
        ));
        let msg = Rgba::from_hex("#12").unwrap_err().to_string();
        assert!(msg.contains("#rrggbb"), "{msg}");
    }

    #[test]
    fn hex_round_trip() {
        for s in ["#000000", "#ffffff", "#1e1f22", "#3574f0cc", "#00000000"] {
            assert_eq!(Rgba::from_hex(s).unwrap().to_hex(), s);
        }
        assert_eq!(Rgba::from_hex("#FFF").unwrap().to_string(), "#ffffff");
    }

    #[test]
    fn serde_uses_hex_strings() {
        let c = Rgba::new(1, 2, 3, 4);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"#01020304\"");
        assert_eq!(serde_json::from_str::<Rgba>(&json).unwrap(), c);
        let err = serde_json::from_str::<Rgba>("\"#zz\"").unwrap_err();
        assert!(err.to_string().contains("invalid hex digit"), "{err}");
        assert!(serde_json::from_str::<Rgba>("42").is_err());
    }

    #[test]
    fn wcag_contrast_matches_reference_values() {
        let ratio = Rgba::BLACK.contrast_ratio(Rgba::WHITE);
        assert!((ratio - 21.0).abs() < 1e-9);
        assert!((Rgba::WHITE.contrast_ratio(Rgba::WHITE) - 1.0).abs() < 1e-9);
        // #767676 on white is the well-known ~4.54:1 AA threshold grey.
        let grey = Rgba::from_hex("#767676").unwrap();
        let r = grey.contrast_ratio(Rgba::WHITE);
        assert!((r - 4.54).abs() < 0.01, "{r}");
        assert!((Rgba::WHITE.contrast_ratio(grey) - r).abs() < 1e-12);
    }

    #[test]
    fn compositing() {
        let half_white = Rgba::new(255, 255, 255, 128);
        assert_eq!(half_white.over(Rgba::BLACK), Rgba::rgb(128, 128, 128));
        assert_eq!(Rgba::TRANSPARENT.over(Rgba::WHITE), Rgba::WHITE);
        assert_eq!(Rgba::rgb(9, 8, 7).over(Rgba::WHITE), Rgba::rgb(9, 8, 7));
        // Fully transparent text has no contrast at all.
        assert!((Rgba::TRANSPARENT.contrast_ratio(Rgba::BLACK) - 1.0).abs() < 1e-9);
        assert_eq!(Rgba::WHITE.to_f32_array(), [1.0, 1.0, 1.0, 1.0]);
    }
}
