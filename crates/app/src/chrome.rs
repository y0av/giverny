//! The theme, applied to Giverny's own chrome.
//!
//! The rail, the settings screen and the overlays used to be painted in
//! hardcoded Monet colours, so picking Gruvbox recoloured the grid and left
//! everything around it unchanged. The accents here are taken from the
//! theme's own ANSI palette — a terminal theme already says what its red,
//! yellow and cyan are, and using them is what makes the chrome belong to it.

use eframe::egui::{self, Color32};
use giverny_term::render::theme::Theme;

#[derive(Debug, Clone, Copy)]
pub struct Chrome {
    /// Panel background: the theme's background, lifted slightly so the rail
    /// reads as a separate surface from the terminal beside it.
    pub panel: Color32,
    pub fg: Color32,
    /// Secondary text — paths, keys, hints.
    pub dim: Color32,
    /// Cyan: selection, headings, "live".
    pub accent: Color32,
    /// Yellow: attention, changed-from-default, warnings.
    pub amber: Color32,
    /// Red: degraded, critical.
    pub poppy: Color32,
    /// Green: healthy.
    pub green: Color32,
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let f = |x: u8, y: u8| {
        (x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}

impl Chrome {
    pub fn from_theme(theme: &Theme) -> Self {
        // Bright variants read better as UI accents on either polarity.
        let pick = |dark: usize, light: usize| {
            if theme.is_light() {
                theme.ansi[light]
            } else {
                theme.ansi[dark]
            }
        };
        Chrome {
            panel: mix(
                theme.bg,
                theme.fg,
                if theme.is_light() { 0.05 } else { 0.06 },
            ),
            fg: theme.fg,
            dim: mix(theme.fg, theme.bg, 0.45),
            accent: pick(14, 6),
            amber: pick(11, 3),
            poppy: pick(9, 1),
            green: pick(10, 2),
        }
    }

    /// Push it into egui, so panels, text fields and buttons follow too.
    pub fn apply(&self, ctx: &egui::Context, theme: &Theme) {
        let mut v = if theme.is_light() {
            egui::Visuals::light()
        } else {
            egui::Visuals::dark()
        };
        v.panel_fill = self.panel;
        v.window_fill = self.panel;
        v.faint_bg_color = mix(self.panel, self.fg, 0.05);
        v.extreme_bg_color = theme.bg;
        // Per-widget strokes rather than `override_text_color`, which is a
        // blunt instrument: it also overrides the hyperlink colour, so links
        // come out looking like plain text.
        v.widgets.noninteractive.fg_stroke.color = self.fg;
        v.widgets.inactive.fg_stroke.color = self.fg;
        v.widgets.hovered.fg_stroke.color = self.fg;
        v.widgets.active.fg_stroke.color = self.fg;
        v.widgets.open.fg_stroke.color = self.fg;
        v.hyperlink_color = self.accent;
        v.selection.bg_fill = mix(self.panel, self.accent, 0.55);
        v.selection.stroke.color = theme.bg;
        v.widgets.noninteractive.bg_fill = self.panel;
        v.widgets.inactive.bg_fill = mix(self.panel, self.fg, 0.10);
        v.widgets.inactive.weak_bg_fill = mix(self.panel, self.fg, 0.07);
        v.widgets.hovered.bg_fill = mix(self.panel, self.fg, 0.18);
        v.widgets.hovered.weak_bg_fill = mix(self.panel, self.fg, 0.14);
        v.widgets.active.bg_fill = mix(self.panel, self.accent, 0.35);
        v.widgets.active.weak_bg_fill = mix(self.panel, self.accent, 0.28);
        ctx.set_visuals(v);
        // egui's floating scrollbars are drawn *over* the last ~10px of the
        // content, so as soon as the rail has enough tabs to scroll, the "+"
        // and close buttons at the right edge end up underneath the bar.
        // Reserve the width instead — only when a bar is actually shown, so
        // short rails keep the full width.
        ctx.all_styles_mut(|s| {
            s.spacing.scroll.floating_allocated_width = s.spacing.scroll.bar_width;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_follows_the_theme_rather_than_one_palette() {
        let monet = Chrome::from_theme(&Theme::monet_dark());
        let gruvbox = Chrome::from_theme(&Theme::gruvbox());
        assert_ne!(monet.panel, gruvbox.panel, "rail background is themed");
        assert_ne!(monet.accent, gruvbox.accent, "accents are themed");
    }

    #[test]
    fn light_themes_get_readable_text() {
        for theme in [Theme::monet_light(), Theme::monet_dark()] {
            let c = Chrome::from_theme(&theme);
            let lum = |x: Color32| x.r() as i32 + x.g() as i32 + x.b() as i32;
            // Text must contrast with the panel it sits on, and `dim` must
            // land between the two rather than vanishing into either.
            assert!(
                (lum(c.fg) - lum(c.panel)).abs() > 150,
                "text too close to the panel"
            );
            let between =
                (lum(c.dim) - lum(c.panel)).abs() > 40 && (lum(c.dim) - lum(c.fg)).abs() > 40;
            assert!(between, "dim text is indistinguishable");
        }
    }
}
