//! Snapshot capture (under the terminal lock) and mesh building (outside it).
//!
//! `Snapshot::capture` copies the visible viewport with colors fully resolved,
//! so mesh building — including first-use glyph rasterization — never holds
//! the terminal lock.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::CursorShape;
use egui::epaint::{Mesh, Rect, TextureId, Vertex, WHITE_UV};
use egui::{Color32, Pos2, Vec2};

use super::atlas::{Atlas, GlyphKey};
use super::metrics::{CellMetrics, FontSet, SYNTH_BOLD, SYNTH_ITALIC};
use super::theme::{Theme, dim};

/// One visible cell with fully resolved colors. `line` is viewport-relative
/// (0 = top row on screen).
#[derive(Debug, Clone)]
pub struct SnapCell {
    pub line: u16,
    pub col: u16,
    pub c: char,
    pub fg: Color32,
    /// `None` when it equals the default background (no quad needed).
    pub bg: Option<Color32>,
    pub flags: Flags,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<SnapCell>,
    /// Viewport-relative cursor; `None` when hidden or scrolled off-screen.
    pub cursor: Option<(u16, u16, CursorShape)>,
    pub display_offset: usize,
    pub total_lines: usize,
    pub mode: TermMode,
}

impl Snapshot {
    pub fn capture<T: EventListener>(term: &Term<T>, theme: &Theme) -> Snapshot {
        use alacritty_terminal::grid::Dimensions;

        let content = term.renderable_content();
        let display_offset = content.display_offset;
        let grid = term.grid();
        let (cols, rows) = (grid.columns(), grid.screen_lines());
        let total_lines = grid.total_lines();

        let selection = content.selection;
        let mut cells = Vec::with_capacity(cols * rows / 2);

        for indexed in content.display_iter {
            let point: Point = indexed.point;
            let vp_line = point.line.0 + display_offset as i32;
            if vp_line < 0 || vp_line >= rows as i32 {
                continue;
            }
            let cell = &*indexed;
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }

            let mut fg = theme.resolve(cell.fg, content.colors);
            let mut bg = theme.resolve(cell.bg, content.colors);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = dim(fg);
            }
            let selected = selection.is_some_and(|s| s.contains(point));
            let mut bg_opt = if bg == theme.bg && !selected { None } else { Some(bg) };
            if selected {
                let s = theme.selection_bg;
                let base = bg_opt.unwrap_or(theme.bg);
                bg_opt = Some(blend(base, s));
            }

            let blank_char = cell.c == ' ' || cell.flags.contains(Flags::HIDDEN);
            if blank_char && bg_opt.is_none() && !cell.flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT) {
                continue;
            }

            cells.push(SnapCell {
                line: vp_line as u16,
                col: point.column.0 as u16,
                c: if blank_char { ' ' } else { cell.c },
                fg,
                bg: bg_opt,
                flags: cell.flags,
            });
        }

        let cursor = {
            let p = content.cursor.point;
            let vp_line = p.line.0 + display_offset as i32;
            let visible = content.cursor.shape != CursorShape::Hidden
                && vp_line >= 0
                && vp_line < rows as i32
                && content.mode.contains(TermMode::SHOW_CURSOR);
            visible.then_some((vp_line as u16, p.column.0 as u16, content.cursor.shape))
        };

        Snapshot {
            cols: cols as u16,
            rows: rows as u16,
            cells,
            cursor,
            display_offset,
            total_lines,
            mode: content.mode,
        }
    }
}

/// Built geometry for one frame, in egui points.
pub struct TermMeshes {
    /// Background quads + cursor block (white-texture mesh).
    pub bg: Mesh,
    /// Glyph quads grouped by atlas page.
    pub glyphs: Vec<Mesh>,
    /// Underlines, strikethrough, beam/underline cursors.
    pub decor: Mesh,
}

pub struct BuildParams<'a> {
    pub ctx: &'a egui::Context,
    pub fonts: &'a FontSet,
    pub atlas: &'a mut Atlas,
    pub metrics: CellMetrics,
    pub theme: &'a Theme,
    /// Widget origin in physical pixels (already rounded to the pixel grid).
    pub origin_px: Vec2,
    pub pixels_per_point: f32,
}

pub fn build(snapshot: &Snapshot, p: &mut BuildParams<'_>) -> TermMeshes {
    let m = p.metrics;
    let ppp = p.pixels_per_point;
    let (cw, ch) = (m.cell_w as f32, m.cell_h as f32);

    let mut bg = Mesh::default();
    let mut decor = Mesh::default();
    let mut glyph_meshes: Vec<Mesh> = Vec::new();

    let cell_rect_px = |line: u16, col: u16| {
        let x = p.origin_px.x + col as f32 * cw;
        let y = p.origin_px.y + line as f32 * ch;
        Rect::from_min_size(Pos2::new(x, y), Vec2::new(cw, ch))
    };
    let to_points = |r: Rect| Rect::from_min_max((r.min.to_vec2() / ppp).to_pos2(), (r.max.to_vec2() / ppp).to_pos2());

    // Cursor block backdrop paints first so glyphs draw over it.
    let cursor_block = snapshot.cursor.filter(|(.., s)| *s == CursorShape::Block);
    if let Some((line, col, _)) = cursor_block {
        push_solid(&mut bg, to_points(cell_rect_px(line, col)), p.theme.cursor);
    }

    for cell in &snapshot.cells {
        let rect_px = cell_rect_px(cell.line, cell.col);
        let under_cursor = cursor_block.is_some_and(|(l, c, _)| l == cell.line && c == cell.col);

        if let Some(bgc) = cell.bg
            && !under_cursor
        {
            let mut r = rect_px;
            if cell.flags.contains(Flags::WIDE_CHAR) {
                r = Rect::from_min_size(r.min, Vec2::new(cw * 2.0, ch));
            }
            push_solid(&mut bg, to_points(r), bgc);
        }

        let fg = if under_cursor { p.theme.cursor_text } else { cell.fg };

        // Glyph.
        if cell.c != ' ' {
            let bold = cell.flags.intersects(Flags::BOLD);
            let italic = cell.flags.intersects(Flags::ITALIC);
            if let Some(r) = p.fonts.resolve(cell.c, bold, italic) {
                let key = GlyphKey {
                    slot: r.slot,
                    glyph: r.glyph,
                    ppem_bits: m.ppem.to_bits(),
                    synth: r.synth
                        | if bold { SYNTH_BOLD & r.synth } else { 0 }
                        | if italic { SYNTH_ITALIC & r.synth } else { 0 },
                };
                let sprite = p.atlas.get(p.ctx, p.fonts, key);
                if !sprite.is_blank() {
                    let pen_x = rect_px.min.x + sprite.left;
                    let pen_y = rect_px.min.y + m.baseline as f32 - sprite.top;
                    let gr = Rect::from_min_size(
                        Pos2::new(pen_x, pen_y),
                        Vec2::new(sprite.size.x, sprite.size.y),
                    );
                    let color = if sprite.is_color { Color32::WHITE } else { fg };
                    let mesh = mesh_for(&mut glyph_meshes, sprite.tex);
                    push_uv(mesh, to_points(gr), sprite.uv_min, sprite.uv_max, color);
                }
            }
        }

        // Decorations.
        let thick = (m.cell_h as f32 / 14.0).max(1.0).round();
        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            let y = rect_px.min.y + (m.baseline as f32 + 2.0).min(ch - thick);
            let r = Rect::from_min_size(Pos2::new(rect_px.min.x, y), Vec2::new(cw, thick));
            push_solid(&mut decor, to_points(r), fg);
            if cell.flags.contains(Flags::DOUBLE_UNDERLINE) {
                let r2 = Rect::from_min_size(
                    Pos2::new(rect_px.min.x, (y - 2.0 * thick).max(rect_px.min.y)),
                    Vec2::new(cw, thick),
                );
                push_solid(&mut decor, to_points(r2), fg);
            }
        }
        if cell.flags.contains(Flags::STRIKEOUT) {
            let y = rect_px.min.y + (ch * 0.5).round();
            let r = Rect::from_min_size(Pos2::new(rect_px.min.x, y), Vec2::new(cw, thick));
            push_solid(&mut decor, to_points(r), fg);
        }
    }

    // Non-block cursors.
    if let Some((line, col, shape)) = snapshot.cursor {
        let rect_px = cell_rect_px(line, col);
        let thick = (m.cell_w as f32 / 6.0).max(1.0).round();
        match shape {
            CursorShape::Beam => {
                let r = Rect::from_min_size(rect_px.min, Vec2::new(thick, ch));
                push_solid(&mut decor, to_points(r), p.theme.cursor);
            }
            CursorShape::Underline => {
                let r = Rect::from_min_size(
                    Pos2::new(rect_px.min.x, rect_px.max.y - thick),
                    Vec2::new(cw, thick),
                );
                push_solid(&mut decor, to_points(r), p.theme.cursor);
            }
            CursorShape::HollowBlock => {
                let r = to_points(rect_px);
                let t = thick / ppp;
                for side in [
                    Rect::from_min_size(r.min, Vec2::new(r.width(), t)),
                    Rect::from_min_size(Pos2::new(r.min.x, r.max.y - t), Vec2::new(r.width(), t)),
                    Rect::from_min_size(r.min, Vec2::new(t, r.height())),
                    Rect::from_min_size(Pos2::new(r.max.x - t, r.min.y), Vec2::new(t, r.height())),
                ] {
                    push_solid(&mut decor, side, p.theme.cursor);
                }
            }
            CursorShape::Block | CursorShape::Hidden => {}
        }
    }

    TermMeshes { bg, glyphs: glyph_meshes, decor }
}

fn mesh_for(meshes: &mut Vec<Mesh>, tex: TextureId) -> &mut Mesh {
    if let Some(i) = meshes.iter().position(|m| m.texture_id == tex) {
        &mut meshes[i]
    } else {
        let mut m = Mesh::default();
        m.texture_id = tex;
        meshes.push(m);
        meshes.last_mut().unwrap()
    }
}

fn push_solid(mesh: &mut Mesh, rect: Rect, color: Color32) {
    push_uv(mesh, rect, Vec2::new(WHITE_UV.x, WHITE_UV.y), Vec2::new(WHITE_UV.x, WHITE_UV.y), color);
}

fn push_uv(mesh: &mut Mesh, rect: Rect, uv_min: Vec2, uv_max: Vec2, color: Color32) {
    let idx = mesh.vertices.len() as u32;
    mesh.vertices.extend_from_slice(&[
        Vertex { pos: rect.left_top(), uv: Pos2::new(uv_min.x, uv_min.y), color },
        Vertex { pos: rect.right_top(), uv: Pos2::new(uv_max.x, uv_min.y), color },
        Vertex { pos: rect.right_bottom(), uv: Pos2::new(uv_max.x, uv_max.y), color },
        Vertex { pos: rect.left_bottom(), uv: Pos2::new(uv_min.x, uv_max.y), color },
    ]);
    mesh.indices.extend_from_slice(&[idx, idx + 1, idx + 2, idx, idx + 2, idx + 3]);
}

fn blend(base: Color32, over: Color32) -> Color32 {
    let a = over.a() as u32;
    let inv = 255 - a;
    Color32::from_rgb(
        ((base.r() as u32 * inv + over.r() as u32 * a) / 255) as u8,
        ((base.g() as u32 * inv + over.g() as u32 * a) / 255) as u8,
        ((base.b() as u32 * inv + over.b() as u32 * a) / 255) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::Event;
    use alacritty_terminal::term::{Config, test::TermSize};
    use alacritty_terminal::vte::ansi::Processor;

    #[derive(Clone)]
    struct NoopProxy;
    impl EventListener for NoopProxy {
        fn send_event(&self, _: Event) {}
    }

    fn term_with(bytes: &[u8]) -> Term<NoopProxy> {
        let mut term = Term::new(Config::default(), &TermSize::new(20, 5), NoopProxy);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    #[test]
    fn snapshot_captures_text_at_viewport_origin() {
        let term = term_with(b"hi");
        let snap = Snapshot::capture(&term, &Theme::monet_dark());
        assert_eq!(snap.cols, 20);
        assert_eq!(snap.rows, 5);
        let h = snap.cells.iter().find(|c| c.c == 'h').expect("h cell");
        assert_eq!((h.line, h.col), (0, 0));
        let i = snap.cells.iter().find(|c| c.c == 'i').expect("i cell");
        assert_eq!((i.line, i.col), (0, 1));
        assert!(snap.cursor.is_some());
        assert_eq!(snap.cursor.unwrap().0, 0);
        assert_eq!(snap.cursor.unwrap().1, 2);
    }

    #[test]
    fn snapshot_resolves_sgr_colors_and_inverse() {
        // red fg "r", then inverse "v"
        let term = term_with(b"\x1b[31mr\x1b[7mv");
        let theme = Theme::monet_dark();
        let snap = Snapshot::capture(&term, &theme);
        let r = snap.cells.iter().find(|c| c.c == 'r').unwrap();
        assert_eq!(r.fg, theme.ansi[1]);
        assert_eq!(r.bg, None, "default bg elided");
        let v = snap.cells.iter().find(|c| c.c == 'v').unwrap();
        assert_eq!(v.bg, Some(theme.ansi[1]), "inverse swaps fg into bg");
        assert_eq!(v.fg, theme.bg);
    }

    #[test]
    fn meshes_have_geometry() {
        let term = term_with(b"\x1b[31mhello\x1b[4m_under");
        let theme = Theme::monet_dark();
        let snap = Snapshot::capture(&term, &theme);

        let ctx = egui::Context::default();
        let fonts = FontSet::load(None).unwrap();
        let metrics = CellMetrics::compute(fonts.primary().as_font().unwrap(), 15.0);
        let mut atlas = Atlas::default();
        let mut params = BuildParams {
            ctx: &ctx,
            fonts: &fonts,
            atlas: &mut atlas,
            metrics,
            theme: &theme,
            origin_px: Vec2::ZERO,
            pixels_per_point: 1.0,
        };
        let meshes = build(&snap, &mut params);

        assert!(!meshes.glyphs.is_empty(), "glyph mesh expected");
        let glyph_quads: usize = meshes.glyphs.iter().map(|m| m.vertices.len() / 4).sum();
        assert!(glyph_quads >= 10, "hello_under = 11 glyphs, got {glyph_quads}");
        assert!(!meshes.decor.vertices.is_empty(), "underline decor expected");
        assert!(!meshes.bg.vertices.is_empty(), "cursor block expected in bg");
    }

    #[test]
    fn scrolled_back_content_maps_into_viewport() {
        // Fill 8 lines in a 5-row term, then scroll display back by 3.
        let mut term = term_with(b"l1\r\nl2\r\nl3\r\nl4\r\nl5\r\nl6\r\nl7\r\nl8");
        term.scroll_display(alacritty_terminal::grid::Scroll::Delta(3));
        let snap = Snapshot::capture(&term, &Theme::monet_dark());
        assert_eq!(snap.display_offset, 3);
        // Topmost visible line should now be "l1" (8 lines, 5 visible, offset 3).
        let top: Vec<&SnapCell> = snap.cells.iter().filter(|c| c.line == 0).collect();
        assert!(top.iter().any(|c| c.c == 'l'), "top row has scrollback text");
        assert!(top.iter().any(|c| c.c == '1'), "expected l1 at top, cells: {top:?}");
    }
}
