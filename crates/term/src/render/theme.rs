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
    /// The chrome's accent, where it is not the theme's bright cyan. Giverny
    /// takes selection and headings from cyan by default, which is wrong for a
    /// theme whose signature colour is something else.
    pub accent: Option<Color32>,
    /// Put every colour a program asks for by value onto one ramp: from the
    /// background, through this colour, to near white. The palette already
    /// speaks the theme's one colour; this is for what bypasses it — 24-bit
    /// colour and the 256-colour cube, Claude Code's diffs among them.
    pub mono: Option<Color32>,
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
            accent: None,
            mono: None,
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
            accent: None,
            mono: None,
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
            accent: None,
            mono: None,
        }
    }

    /// Tokyo Night — the widely used dark blue-purple palette.
    pub fn tokyo_night() -> Self {
        Theme::from_hex(
            0x1a1b26,
            0xc0caf5,
            0xc0caf5,
            0x1a1b26,
            [
                0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xa9b1d6,
                0x414868, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5,
            ],
        )
    }

    /// Gruvbox dark.
    pub fn gruvbox() -> Self {
        Theme::from_hex(
            0x282828,
            0xebdbb2,
            0xebdbb2,
            0x282828,
            [
                0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
            ],
        )
    }

    /// Nord.
    pub fn nord() -> Self {
        Theme::from_hex(
            0x2e3440,
            0xd8dee9,
            0xd8dee9,
            0x2e3440,
            [
                0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
            ],
        )
    }

    /// Catppuccin Mocha.
    pub fn catppuccin() -> Self {
        Theme::from_hex(
            0x1e1e2e,
            0xcdd6f4,
            0xf5e0dc,
            0x1e1e2e,
            [
                0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
            ],
        )
    }

    /// Catppuccin Mocha as the Colloid GTK theme wears it on a GNOME desktop:
    /// mauve where stock Mocha uses teal and rosewater, Latte's near-white
    /// for text, and magenta given to mauve so programs pick it up too.
    pub fn catppuccin_mauve() -> Self {
        let mauve = hex(0xcba6f7);
        Theme {
            accent: Some(mauve),
            selection_bg: Color32::from_rgba_unmultiplied(0xcb, 0xa6, 0xf7, 70),
            ..Theme::from_hex(
                0x1e1e2e,
                0xeff1f5,
                0xcba6f7,
                0x1e1e2e,
                [
                    0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xcba6f7, 0x94e2d5, 0xbac2de,
                    0x6c7086, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0xb4befe, 0xf5c2e7, 0x94e2d5, 0xeff1f5,
                ],
            )
        }
    }

    /// P3 amber phosphor: one colour at every brightness, and whatever a
    /// program sends in 24-bit colour goes amber with it.
    pub fn phosphor() -> Self {
        Theme {
            mono: Some(hex(0xffb000)),
            ..Theme::from_hex(
                0x120b00,
                0xffb000,
                0xffb000,
                0x120b00,
                [
                    0x2a1a00, 0xffcc66, 0xd99400, 0xffb000, 0xb07800, 0xe0a030, 0xffc040, 0xe6a200,
                    0x9a6a10, 0xffe0a0, 0xffb84d, 0xffd480, 0xcc9020, 0xffc870, 0xffd890, 0xfff0cc,
                ],
            )
        }
    }

    /// Bioluminescence: black water, and colour that makes its own light.
    pub fn abyss() -> Self {
        Theme::from_hex(
            0x03080d,
            0xbfeee8,
            0x3ff5e0,
            0x03080d,
            [
                0x0c1a22, 0xff5c8a, 0x3ff5a0, 0xe8f55a, 0x3a7bff, 0x9d7bff, 0x3ff5e0, 0x9fc9c4,
                0x4a7680, 0xff85a8, 0x7affc2, 0xf0ff8a, 0x7aa6ff, 0xbda6ff, 0x7afff0, 0xe8fffc,
            ],
        )
    }

    /// Neon on a violet sky.
    pub fn synthwave() -> Self {
        Theme::from_hex(
            0x1a0b2e,
            0xf4e6ff,
            0xff3fd0,
            0x1a0b2e,
            [
                0x2a1648, 0xff3864, 0x36f9b4, 0xffe45e, 0x5d5dff, 0xff3fd0, 0x36d9f9, 0xd8c8f0,
                0x7a66a6, 0xff6b8b, 0x72ffd0, 0xfff08a, 0x8a8aff, 0xff7ae0, 0x72ecff, 0xffffff,
            ],
        )
    }

    /// Amiga Workbench 1.3: blue, white, black, and orange for what matters.
    pub fn workbench() -> Self {
        Theme {
            accent: Some(hex(0xff8800)),
            ..Theme::from_hex(
                0x0055aa,
                0xffffff,
                0xff8800,
                0x0055aa,
                [
                    0x000022, 0xff6a3d, 0x6be07a, 0xffcc33, 0x66aaff, 0xff88dd, 0x88ddff, 0xdddddd,
                    0x9dbbe8, 0xff8a66, 0x9af0a6, 0xffdd66, 0x99c8ff, 0xffaaee, 0xaae8ff, 0xffffff,
                ],
            )
        }
    }

    /// Risograph fluorescent inks on black paper.
    pub fn riso() -> Self {
        Theme::from_hex(
            0x16130f,
            0xf2ece0,
            0xff48b0,
            0x16130f,
            [
                0x26211b, 0xff6c2f, 0x2fc47a, 0xffe800, 0x4a86e8, 0xff48b0, 0x3fc1c9, 0xd8d0c2,
                0x7a7264, 0xff8a5c, 0x5fe09a, 0xfff066, 0x7aa8ff, 0xff7cc8, 0x6fe0e6, 0xfffaf0,
            ],
        )
    }

    /// Pastels on terracotta: the ground is the colour, not a tint of black.
    /// Red Rock by Mark Badolato, from iTerm2-Color-Schemes (MIT).
    pub fn red_rock() -> Self {
        Theme::from_hex(
            0x672a20,
            0xfff0dc,
            0xf7bd55,
            0x672a20,
            [
                0xd9c7bb, 0xffa18b, 0x91d7a7, 0xf5d06f, 0x94c9ee, 0xe7a9d3, 0x87ddd7, 0xfff0dc,
                0xe6d5ca, 0xffb5a1, 0xaae1ba, 0xffe08e, 0xb0d8f3, 0xefc0de, 0xa7e8e2, 0xfffaf2,
            ],
        )
    }

    /// Monet's Rouen Cathedral series — one façade in more than thirty
    /// lights — as a theme that follows the clock. `hour` is local time,
    /// 0.0 to 24.0; the light between two of these is a blend of both, so the
    /// palette never jumps.
    pub fn rouen_at(hour: f32) -> Self {
        const LIGHTS: [(f32, u32, u32, u32, [u32; 16]); 4] = [
            // Morning fog.
            (
                6.0,
                0x15181f,
                0xdde2ec,
                0xc8b8e8,
                [
                    0x20242c, 0xc9727a, 0x8fae9a, 0xd4c28e, 0x7c93c4, 0xa693c8, 0x7fb0bf, 0xcfd4de,
                    0x5d6678, 0xe08e95, 0xa8c8b4, 0xe8d8a8, 0x9ab0dc, 0xc0aee0, 0x9ccbd8, 0xeef1f7,
                ],
            ),
            // Full sun.
            (
                12.0,
                0x1d160b,
                0xf0e3c6,
                0xf2c14e,
                [
                    0x2a2114, 0xd0643c, 0x9bb05a, 0xf2c14e, 0x6a8fc0, 0xc07ab0, 0x6fb3a8, 0xe3d4b4,
                    0x7d6c50, 0xec8458, 0xb8cc78, 0xffd978, 0x8fb0dc, 0xdc9ccc, 0x8fd0c4, 0xfff4dc,
                ],
            ),
            // Evening.
            (
                18.0,
                0x1f1117,
                0xf3dcd8,
                0xff9a6b,
                [
                    0x2d1a22, 0xe5604f, 0xa0a86a, 0xf0a860, 0x7b7fc0, 0xd07aa8, 0x7aaab0, 0xe0c8c4,
                    0x80606c, 0xff8070, 0xbcc488, 0xffc488, 0x9ca0dc, 0xec9cc4, 0x9ccacc, 0xfff0ec,
                ],
            ),
            // Night, which Monet did not paint and a terminal needs.
            (
                23.0,
                0x0a0e1c,
                0xcdd3e6,
                0x8fa8ff,
                [
                    0x141a2c, 0xc05a6a, 0x6f9a86, 0xc8b070, 0x4f6fc0, 0x8f78c0, 0x5a9ab0, 0xb8bfd4,
                    0x4f5a78, 0xd8788a, 0x8cb8a2, 0xe0cc90, 0x7090e0, 0xac98dc, 0x7ebcd0, 0xe6ebf7,
                ],
            ),
        ];
        let light = |i: usize| {
            let (_, bg, fg, cursor, ansi) = LIGHTS[i % LIGHTS.len()];
            Theme::from_hex(bg, fg, cursor, bg, ansi)
        };
        // Round the clock: before the first light is still the night before.
        let first = LIGHTS[0].0;
        let at = if hour.rem_euclid(24.0) < first {
            hour.rem_euclid(24.0) + 24.0
        } else {
            hour.rem_euclid(24.0)
        };
        let i = (0..LIGHTS.len())
            .rev()
            .find(|&i| at >= LIGHTS[i].0)
            .unwrap_or(0);
        let from = LIGHTS[i].0;
        let to = if i + 1 < LIGHTS.len() {
            LIGHTS[i + 1].0
        } else {
            first + 24.0
        };
        light(i).blend(&light(i + 1), (at - from) / (to - from))
    }

    /// Every colour `t` of the way from this theme to `other`.
    fn blend(&self, other: &Theme, t: f32) -> Theme {
        let mix = |a: Color32, b: Color32| mix(a, b, t);
        Theme {
            bg: mix(self.bg, other.bg),
            fg: mix(self.fg, other.fg),
            cursor: mix(self.cursor, other.cursor),
            cursor_text: mix(self.cursor_text, other.cursor_text),
            selection_bg: {
                let c = mix(self.selection_bg, other.selection_bg);
                Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 80)
            },
            ansi: std::array::from_fn(|i| mix(self.ansi[i], other.ansi[i])),
            accent: None,
            mono: None,
        }
    }

    fn from_hex(bg: u32, fg: u32, cursor: u32, cursor_text: u32, ansi: [u32; 16]) -> Theme {
        let hex =
            |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8);
        let sel = hex(ansi[4]);
        Theme {
            bg: hex(bg),
            fg: hex(fg),
            cursor: hex(cursor),
            cursor_text: hex(cursor_text),
            selection_bg: Color32::from_rgba_unmultiplied(sel.r(), sel.g(), sel.b(), 80),
            ansi: ansi.map(hex),
            accent: None,
            mono: None,
        }
    }

    /// How far a worker's background leans toward the theme's magenta.
    pub const WORKER_TINT: f32 = 0.12;

    /// The background a tab shows while it is a worker rather than the
    /// manager: a Claude Code subagent view, or a tab that
    /// follows a worker's transcript. A shift of this theme's own background
    /// toward its own magenta, so it reads on light and dark themes alike,
    /// follows a theme change (Rouen's hour included), and is not the accent
    /// the chrome already uses for selection.
    pub fn worker_bg(&self) -> Color32 {
        mix(self.bg, self.ansi[5], Self::WORKER_TINT)
    }

    /// Every built-in, in picker order. The names are the values of
    /// `theme.name`, and the settings schema is checked against this list.
    pub const NAMES: &'static [&'static str] = &[
        "monet-dark",
        "monet-light",
        "ink",
        "tokyo-night",
        "gruvbox",
        "nord",
        "catppuccin",
        "catppuccin-mauve",
        "rouen",
        "phosphor",
        "abyss",
        "synthwave",
        "workbench",
        "riso",
        "red-rock",
    ];

    /// Look up a built-in theme by config name.
    pub fn by_name(name: &str) -> Theme {
        match name {
            "monet-light" | "light" => Theme::monet_light(),
            "ink" => Theme::ink(),
            "tokyo-night" => Theme::tokyo_night(),
            "gruvbox" => Theme::gruvbox(),
            "nord" => Theme::nord(),
            "catppuccin" => Theme::catppuccin(),
            "catppuccin-mauve" => Theme::catppuccin_mauve(),
            // The app asks for the hour it actually is; see `rouen_at`.
            "rouen" => Theme::rouen_at(12.0),
            "phosphor" => Theme::phosphor(),
            "abyss" => Theme::abyss(),
            "synthwave" => Theme::synthwave(),
            "workbench" => Theme::workbench(),
            "riso" => Theme::riso(),
            "red-rock" => Theme::red_rock(),
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
            AnsiColor::Spec(rgb) => self.onto_ramp(to32(rgb)),
            AnsiColor::Indexed(i) => match overrides[i as usize] {
                Some(rgb) => self.onto_ramp(to32(rgb)),
                // The theme's own sixteen are already on it.
                None if i < 16 => self.indexed(i),
                None => self.onto_ramp(self.indexed(i)),
            },
            AnsiColor::Named(n) => match overrides[n] {
                Some(rgb) => self.onto_ramp(to32(rgb)),
                None => self.named(n),
            },
        }
    }

    /// A colour from outside the palette, for a `mono` theme: its brightness
    /// kept, its hue replaced. Dark goes to the background, mid-grey to the
    /// theme's colour, white to that colour washed nearly white — so a red
    /// diff and a green one stay two different bands, just not two hues.
    fn onto_ramp(&self, c: Color32) -> Color32 {
        let Some(peak) = self.mono else { return c };
        let lum = (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0;
        const KNEE: f32 = 0.6;
        if lum < KNEE {
            mix(self.bg, peak, lum / KNEE)
        } else {
            mix(
                peak,
                mix(peak, Color32::WHITE, 0.8),
                (lum - KNEE) / (1.0 - KNEE),
            )
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

fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8 & 0xff) as u8, (v & 0xff) as u8)
}

/// `t` of the way from `a` to `b`, per channel.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
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
        assert!(!Theme::by_name("red-rock").is_light());
        assert!(
            !Theme::by_name("nonsense").is_light(),
            "unknown names fall back to the default dark theme"
        );
    }

    /// Rouen never jumps: the palette on either side of any moment is the
    /// same colour to within a rounding step, midnight included.
    #[test]
    fn rouen_moves_without_jumping() {
        let near = |a: Color32, b: Color32| {
            (a.r() as i32 - b.r() as i32).abs() <= 2
                && (a.g() as i32 - b.g() as i32).abs() <= 2
                && (a.b() as i32 - b.b() as i32).abs() <= 2
        };
        let mut h = 0.0f32;
        while h < 24.0 {
            let (a, b) = (Theme::rouen_at(h), Theme::rouen_at(h + 0.05));
            assert!(near(a.bg, b.bg), "background jumps at {h}");
            assert!(near(a.fg, b.fg), "text jumps at {h}");
            h += 0.05;
        }
        // Each light is itself at its own hour, and noon is not night.
        assert_eq!(
            Theme::rouen_at(12.0).bg,
            Color32::from_rgb(0x1d, 0x16, 0x0b)
        );
        assert_ne!(Theme::rouen_at(12.0).bg, Theme::rouen_at(0.0).bg);
        assert!(
            !Theme::rouen_at(3.0).is_light(),
            "it stays a dark theme all night"
        );
    }

    /// What comes in by value lands on the amber ramp; the palette is left
    /// alone because it is already there.
    #[test]
    fn phosphor_puts_every_colour_on_one_ramp() {
        let t = Theme::phosphor();
        let none = Colors::default();
        let red = t.resolve(
            AnsiColor::Spec(Rgb {
                r: 122,
                g: 41,
                b: 54,
            }),
            &none,
        );
        let green = t.resolve(
            AnsiColor::Spec(Rgb {
                r: 34,
                g: 92,
                b: 43,
            }),
            &none,
        );
        let amber = |c: Color32| c.r() >= c.g() && c.g() >= c.b();
        assert!(amber(red) && amber(green), "{red:?} {green:?}");
        assert_ne!(red, green, "two diffs stay two bands");
        assert_eq!(t.resolve(AnsiColor::Indexed(1), &none), t.ansi[1]);
        // A theme without a ramp passes colour through untouched.
        let rgb = Rgb { r: 1, g: 2, b: 3 };
        assert_eq!(
            Theme::monet_dark().resolve(AnsiColor::Spec(rgb), &none),
            to32(rgb)
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
