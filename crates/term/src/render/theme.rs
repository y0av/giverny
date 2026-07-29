//! Terminal color theme and palette resolution.
//!
//! Resolution order: explicit RGB from the program → runtime palette
//! overrides (OSC 4 etc., via `Term::colors()`) → theme defaults.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use egui::Color32;

#[derive(Debug, Clone)]
pub struct Theme {
    pub bg: Color32,
    pub fg: Color32,
    pub cursor: Color32,
    pub cursor_text: Color32,
    pub selection_bg: Color32,
    pub ansi: [Color32; 16],
}

impl Theme {
    /// Default dark theme, tinted after Monet's lily pond.
    pub fn monet_dark() -> Self {
        let hex =
            |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8);
        Theme {
            bg: hex(0x0e1417),
            fg: hex(0xd7dde2),
            cursor: hex(0xe3c47c),
            cursor_text: hex(0x0e1417),
            selection_bg: Color32::from_rgba_unmultiplied(0x5b, 0x7f, 0xa6, 90),
            ansi: [
                hex(0x1b2427), // black
                hex(0xc35b4e), // red — poppy
                hex(0x7ba25a), // green — garden
                hex(0xd9b55f), // yellow — light
                hex(0x5b7fa6), // blue — pond
                hex(0x9a86b8), // magenta — wisteria
                hex(0x5fa3a3), // cyan — water
                hex(0xc9d1d4), // white
                hex(0x46545a), // bright black
                hex(0xd97f70), // bright red
                hex(0x9bc27b), // bright green
                hex(0xe8cd87), // bright yellow
                hex(0x82a5cc), // bright blue
                hex(0xb8a6d6), // bright magenta
                hex(0x84c5c5), // bright cyan
                hex(0xe8eef1), // bright white
            ],
        }
    }

    /// Daylight version of the garden palette.
    pub fn monet_light() -> Self {
        let hex =
            |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8);
        Theme {
            bg: hex(0xf7f4ec),
            fg: hex(0x2f3438),
            cursor: hex(0x9a6b1f),
            cursor_text: hex(0xf7f4ec),
            selection_bg: Color32::from_rgba_unmultiplied(0x5b, 0x7f, 0xa6, 70),
            ansi: [
                hex(0x2f3438),
                hex(0xa8412f),
                hex(0x4d7a34),
                hex(0x9a7418),
                hex(0x2f5f8c),
                hex(0x74589c),
                hex(0x2c7d7d),
                hex(0x6d7379),
                hex(0x5b6167),
                hex(0xc35b4e),
                hex(0x7ba25a),
                hex(0xc09a3a),
                hex(0x5b7fa6),
                hex(0x9a86b8),
                hex(0x5fa3a3),
                hex(0x2f3438),
            ],
        }
    }

    /// High-contrast near-monochrome.
    pub fn ink() -> Self {
        let hex =
            |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8);
        Theme {
            bg: hex(0x0b0b0c),
            fg: hex(0xe6e6e6),
            cursor: hex(0xffffff),
            cursor_text: hex(0x0b0b0c),
            selection_bg: Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 60),
            ansi: [
                hex(0x1c1c1e),
                hex(0xd06b5c),
                hex(0x8fb573),
                hex(0xd8c07a),
                hex(0x7f9ec4),
                hex(0xa694c4),
                hex(0x76b8b8),
                hex(0xc8c8c8),
                hex(0x5a5a5e),
                hex(0xe8897a),
                hex(0xa9d18d),
                hex(0xf0dc9a),
                hex(0x9db9dc),
                hex(0xc0b0dc),
                hex(0x96d2d2),
                hex(0xf5f5f5),
            ],
        }
    }

    /// Look up a built-in theme by config name.
    pub fn by_name(name: &str) -> Theme {
        match name {
            "monet-light" | "light" => Theme::monet_light(),
            "ink" => Theme::ink(),
            _ => Theme::monet_dark(),
        }
    }

    /// True when the background is light (UI chrome follows).
    pub fn is_light(&self) -> bool {
        let c = self.bg;
        (c.r() as u32 + c.g() as u32 + c.b() as u32) / 3 > 127
    }

    /// Resolve a VT color against runtime overrides and this theme.
    pub fn resolve(&self, color: AnsiColor, overrides: &Colors) -> Color32 {
        match color {
            AnsiColor::Spec(rgb) => to32(rgb),
            AnsiColor::Indexed(i) => match overrides[i as usize] {
                Some(rgb) => to32(rgb),
                None => self.indexed(i),
            },
            AnsiColor::Named(n) => match overrides[n] {
                Some(rgb) => to32(rgb),
                None => self.named(n),
            },
        }
    }

    pub fn indexed(&self, i: u8) -> Color32 {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let c = i as u32 - 16;
                let comp = |v: u32| if v == 0 { 0u8 } else { (55 + 40 * v) as u8 };
                Color32::from_rgb(comp(c / 36), comp(c / 6 % 6), comp(c % 6))
            }
            232..=255 => {
                let g = (8 + 10 * (i as u32 - 232)) as u8;
                Color32::from_rgb(g, g, g)
            }
        }
    }

    fn named(&self, n: NamedColor) -> Color32 {
        use NamedColor::*;
        match n {
            Foreground | BrightForeground => self.fg,
            Background => self.bg,
            Cursor => self.cursor,
            Black => self.ansi[0],
            Red => self.ansi[1],
            Green => self.ansi[2],
            Yellow => self.ansi[3],
            Blue => self.ansi[4],
            Magenta => self.ansi[5],
            Cyan => self.ansi[6],
            White => self.ansi[7],
            BrightBlack => self.ansi[8],
            BrightRed => self.ansi[9],
            BrightGreen => self.ansi[10],
            BrightYellow => self.ansi[11],
            BrightBlue => self.ansi[12],
            BrightMagenta => self.ansi[13],
            BrightCyan => self.ansi[14],
            BrightWhite => self.ansi[15],
            DimForeground => dim(self.fg),
            DimBlack => dim(self.ansi[0]),
            DimRed => dim(self.ansi[1]),
            DimGreen => dim(self.ansi[2]),
            DimYellow => dim(self.ansi[3]),
            DimBlue => dim(self.ansi[4]),
            DimMagenta => dim(self.ansi[5]),
            DimCyan => dim(self.ansi[6]),
            DimWhite => dim(self.ansi[7]),
        }
    }
}

pub fn dim(c: Color32) -> Color32 {
    Color32::from_rgb(
        (c.r() as u32 * 2 / 3) as u8,
        (c.g() as u32 * 2 / 3) as u8,
        (c.b() as u32 * 2 / 3) as u8,
    )
}

fn to32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_cube_and_gray() {
        let t = Theme::monet_dark();
        assert_eq!(t.indexed(16), Color32::from_rgb(0, 0, 0));
        assert_eq!(t.indexed(231), Color32::from_rgb(255, 255, 255));
        assert_eq!(t.indexed(232), Color32::from_rgb(8, 8, 8));
        assert_eq!(t.indexed(255), Color32::from_rgb(238, 238, 238));
        assert_eq!(t.indexed(1), t.ansi[1]);
    }

    #[test]
    fn named_themes_resolve_and_report_lightness() {
        assert!(!Theme::by_name("monet-dark").is_light());
        assert!(Theme::by_name("monet-light").is_light());
        assert!(!Theme::by_name("ink").is_light());
        assert!(
            !Theme::by_name("nonsense").is_light(),
            "unknown names fall back to the default dark theme"
        );
    }

    #[test]
    fn overrides_win() {
        let t = Theme::monet_dark();
        let mut overrides = Colors::default();
        overrides[1] = Some(Rgb { r: 1, g: 2, b: 3 });
        assert_eq!(
            t.resolve(AnsiColor::Indexed(1), &overrides),
            Color32::from_rgb(1, 2, 3)
        );
        assert_eq!(t.resolve(AnsiColor::Indexed(2), &overrides), t.ansi[2]);
    }
}
