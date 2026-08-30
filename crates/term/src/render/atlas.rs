//! Glyph atlas: swash rasterization into shelf-packed RGBA pages uploaded as
//! egui textures. Monochrome glyphs are stored premultiplied-white (vertex
//! color tints them); color emoji are stored as-is and drawn untinted.

use std::collections::HashMap;
use std::sync::Arc;

use egui::epaint::{Color32, ColorImage, ImageData, ImageDelta, TextureId};
use egui::{TextureOptions, Vec2};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::zeno::Format;

use super::metrics::{FontSet, SYNTH_BOLD};

pub const PAGE_SIZE: usize = 1024;
/// Pixels of padding between packed glyphs (bleed guard).
const PAD: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub slot: u16,
    pub glyph: u16,
    /// Exact ppem bits (f32::to_bits) — cells are integer-px so no subpixel bins.
    pub ppem_bits: u32,
    pub synth: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct Sprite {
    pub tex: TextureId,
    /// Normalized UV rect within the page.
    pub uv_min: Vec2,
    pub uv_max: Vec2,
    /// Bitmap size in physical pixels; zero for blank glyphs (spaces).
    pub size: Vec2,
    /// Placement: offset from the pen position (cell x, baseline y).
    pub left: f32,
    pub top: f32,
    /// Color content (emoji) — drawn untinted.
    pub is_color: bool,
}

impl Sprite {
    pub fn is_blank(&self) -> bool {
        self.size.x <= 0.0 || self.size.y <= 0.0
    }
}

struct Page {
    tex: TextureId,
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
}

impl Page {
    /// Reserve a `w`×`h` region; `None` when the page is full.
    fn reserve(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (w, h) = (w + PAD, h + PAD);
        let page = PAGE_SIZE as u32;
        if w > page {
            return None;
        }
        if self.shelf_x + w > page {
            // New shelf.
            let next_y = self.shelf_y + self.shelf_h;
            if next_y + h > page {
                return None;
            }
            self.shelf_x = 0;
            self.shelf_y = next_y;
            self.shelf_h = 0;
        }
        if self.shelf_y + h > page {
            return None;
        }
        let pos = (self.shelf_x, self.shelf_y);
        self.shelf_x += w;
        self.shelf_h = self.shelf_h.max(h);
        Some(pos)
    }
}

pub struct Atlas {
    scale_ctx: ScaleContext,
    pages: Vec<Page>,
    map: HashMap<GlyphKey, Sprite>,
}

impl Default for Atlas {
    fn default() -> Self {
        Self {
            scale_ctx: ScaleContext::new(),
            pages: Vec::new(),
            map: HashMap::new(),
        }
    }
}

impl Atlas {
    /// Number of glyphs rasterized so far (diagnostics).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Get (or rasterize + upload) the sprite for a glyph.
    pub fn get(&mut self, ctx: &egui::Context, fonts: &FontSet, key: GlyphKey) -> Sprite {
        if let Some(sprite) = self.map.get(&key) {
            return *sprite;
        }
        let sprite = self.rasterize_and_pack(ctx, fonts, key);
        self.map.insert(key, sprite);
        sprite
    }

    fn rasterize_and_pack(
        &mut self,
        ctx: &egui::Context,
        fonts: &FontSet,
        key: GlyphKey,
    ) -> Sprite {
        let blank = Sprite {
            tex: TextureId::default(),
            uv_min: Vec2::ZERO,
            uv_max: Vec2::ZERO,
            size: Vec2::ZERO,
            left: 0.0,
            top: 0.0,
            is_color: false,
        };

        let ppem = f32::from_bits(key.ppem_bits);
        let Some(font) = fonts.font(key.slot).as_font() else {
            return blank;
        };

        let mut scaler = self.scale_ctx.builder(font).size(ppem).hint(true).build();
        let mut render = Render::new(&[
            Source::ColorOutline(0),
            Source::ColorBitmap(StrikeWith::BestFit),
            Source::Outline,
        ]);
        render.format(Format::Alpha);
        if key.synth & SYNTH_BOLD != 0 {
            render.embolden(ppem / 16.0);
        }
        let Some(image) = render.render(&mut scaler, key.glyph) else {
            return blank;
        };

        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 {
            return blank;
        }

        // Convert to premultiplied RGBA.
        let mut pixels: Vec<Color32> = Vec::with_capacity((w * h) as usize);
        let is_color = match image.content {
            Content::Mask => {
                pixels.extend(image.data.iter().map(|&a| Color32::from_white_alpha(a)));
                false
            }
            Content::Color => {
                pixels.extend(
                    image
                        .data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|px| Color32::from_rgba_unmultiplied(px[0], px[1], px[2], px[3])),
                );
                true
            }
            Content::SubpixelMask => {
                // We never request subpixel; treat the green channel as coverage.
                pixels.extend(
                    image
                        .data
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|px| Color32::from_white_alpha(px[1])),
                );
                false
            }
        };
        if pixels.len() != (w * h) as usize {
            return blank;
        }

        let (page_idx, x, y) = self.reserve(ctx, w, h);
        let page = &self.pages[page_idx];

        let sub = ColorImage {
            size: [w as usize, h as usize],
            source_size: Vec2::new(w as f32, h as f32),
            pixels,
        };
        ctx.tex_manager().write().set(
            page.tex,
            ImageDelta::partial([x as usize, y as usize], sub, TextureOptions::NEAREST),
        );

        let inv = 1.0 / PAGE_SIZE as f32;
        Sprite {
            tex: page.tex,
            uv_min: Vec2::new(x as f32 * inv, y as f32 * inv),
            uv_max: Vec2::new((x + w) as f32 * inv, (y + h) as f32 * inv),
            size: Vec2::new(w as f32, h as f32),
            left: image.placement.left as f32,
            top: image.placement.top as f32,
            is_color,
        }
    }

    fn reserve(&mut self, ctx: &egui::Context, w: u32, h: u32) -> (usize, u32, u32) {
        if let Some(last) = self.pages.len().checked_sub(1)
            && let Some((x, y)) = self.pages[last].reserve(w, h)
        {
            return (last, x, y);
        }
        // Allocate a new page.
        let image = ColorImage::filled([PAGE_SIZE, PAGE_SIZE], Color32::TRANSPARENT);
        let tex = ctx.tex_manager().write().alloc(
            format!("giverny-atlas-{}", self.pages.len()),
            ImageData::Color(Arc::new(image)),
            TextureOptions::NEAREST,
        );
        let mut page = Page {
            tex,
            shelf_x: 0,
            shelf_y: 0,
            shelf_h: 0,
        };
        let (x, y) = page.reserve(w, h).expect("glyph larger than an atlas page");
        self.pages.push(page);
        (self.pages.len() - 1, x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::metrics::CellMetrics;

    #[test]
    fn rasterizes_and_caches_a_glyph() {
        let ctx = egui::Context::default();
        let fonts = FontSet::load(None).unwrap();
        let m = CellMetrics::compute(fonts.primary().as_font().unwrap(), 16.0);
        let r = fonts.resolve('A', false, false).unwrap();
        let key = GlyphKey {
            slot: r.slot,
            glyph: r.glyph,
            ppem_bits: m.ppem.to_bits(),
            synth: r.synth,
        };

        let mut atlas = Atlas::default();
        let sprite = atlas.get(&ctx, &fonts, key);
        assert!(!sprite.is_blank(), "glyph 'A' must have coverage");
        assert!(sprite.size.x > 2.0 && sprite.size.y > 4.0);
        assert_eq!(atlas.len(), 1);
        assert_eq!(atlas.page_count(), 1);

        let again = atlas.get(&ctx, &fonts, key);
        assert_eq!(atlas.len(), 1, "second lookup must hit the cache");
        assert_eq!(again.uv_min, sprite.uv_min);
    }

    #[test]
    fn space_is_blank_and_cached() {
        let ctx = egui::Context::default();
        let fonts = FontSet::load(None).unwrap();
        let r = fonts.resolve(' ', false, false).unwrap();
        let key = GlyphKey {
            slot: r.slot,
            glyph: r.glyph,
            ppem_bits: 16.0f32.to_bits(),
            synth: 0,
        };
        let mut atlas = Atlas::default();
        assert!(atlas.get(&ctx, &fonts, key).is_blank());
        assert_eq!(
            atlas.page_count(),
            0,
            "blank glyphs must not allocate pages"
        );
    }

    #[test]
    fn many_glyphs_pack_into_one_page() {
        let ctx = egui::Context::default();
        let fonts = FontSet::load(None).unwrap();
        let mut atlas = Atlas::default();
        for ch in ('!'..='~').chain('─'..='╿') {
            if let Some(r) = fonts.resolve(ch, false, false) {
                let key = GlyphKey {
                    slot: r.slot,
                    glyph: r.glyph,
                    ppem_bits: 15.0f32.to_bits(),
                    synth: r.synth,
                };
                atlas.get(&ctx, &fonts, key);
            }
        }
        assert!(atlas.len() > 90);
        assert_eq!(
            atlas.page_count(),
            1,
            "ascii + box drawing fits one 1024² page"
        );
    }
}
